//! End-to-end tests: real upstream `git` against a live gitcask-server backed
//! by the in-memory store. Covers clone/push/fetch (v2 and v0), non-ff reject,
//! ref delete, tags, partial clone + lazy fetch, ls-remote, and the two-instance
//! consistency test (push on A, immediate clone on B). LFS is exercised when
//! `git lfs` is present.
mod harness;

type TestResult = anyhow::Result<()>;
use anyhow::Context;
use base64::Engine as _;
use futures::StreamExt;
use gitcask_store::ObjectStore;
use harness::{Server, TestRepo, git, git_in};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

static INSTALL_DELAY_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn archive_cache_count(server: &Server) -> anyhow::Result<usize> {
    let objects = server
        .store
        .list("repos/api/writes/cache/archive/v1/", None)
        .collect::<Vec<_>>()
        .await;
    for object in &objects {
        object
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    }
    Ok(objects.len())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_api_refs_tags_and_archives() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("api", "writes").await?;
    let src = TestRepo::synthetic(2, 2)?;
    git_in(
        &src,
        &[
            "push",
            "-q",
            &server.repo_url("api", "writes"),
            "main:refs/heads/main",
        ],
    )?;
    let first = git_in(&src, &["rev-parse", "main~1"])?.trim().to_string();
    let head = git_in(&src, &["rev-parse", "main"])?.trim().to_string();
    let zero = "0".repeat(head.len());
    let client = reqwest::Client::new();
    let api = format!("{}/api/writes/api", server.base_url);

    let blob = git_in(&src, &["rev-parse", "main:f1_0.txt"])?
        .trim()
        .to_string();
    assert_eq!(
        client
            .put(format!("{api}/refs/heads/not-a-commit"))
            .json(&serde_json::json!({ "target": blob }))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::BAD_REQUEST
    );
    assert_eq!(
        client
            .put(format!("{api}/refs/heads/missing-target"))
            .json(&serde_json::json!({ "target": "does-not-exist" }))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );

    // Branch create, CAS move, stale-CAS rejection, force move, and delete.
    let response = client
        .put(format!("{api}/refs/heads/api/branch"))
        .json(&serde_json::json!({
            "target": &first,
            "expected_old_oid": zero,
        }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(
        server
            .ls_remote("api", "writes")
            .await?
            .contains(&format!("{first}\trefs/heads/api/branch"))
    );
    let response = client
        .put(format!("{api}/refs/heads/api/branch"))
        .json(&serde_json::json!({
            "target": &head,
            "expected_old_oid": &first,
        }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response = client
        .put(format!("{api}/refs/heads/api/branch"))
        .json(&serde_json::json!({
            "target": &first,
            "expected_old_oid": &first,
        }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    let response = client
        .put(format!("{api}/refs/heads/api/branch"))
        .json(&serde_json::json!({ "target": &first }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response = client
        .delete(format!(
            "{api}/refs/heads/api/branch?expected_old_oid={first}"
        ))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(
        !server
            .ls_remote("api", "writes")
            .await?
            .contains("api/branch")
    );

    // Lightweight and annotated tags use their distinct object shapes.
    let response = client
        .put(format!("{api}/refs/tags/light"))
        .json(&serde_json::json!({ "target": "main" }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response = client
        .post(format!("{api}/tags"))
        .json(&serde_json::json!({
            "name": "api-v1",
            "target": "main",
            "message": "release from API",
            "tagger": {
                "name": "API Tester",
                "email": "api@example.test",
                "when": "2026-09-01T12:34:56+09:00"
            }
        }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let mutation: serde_json::Value = response.json().await?;
    assert_eq!(mutation["peeled"], head);
    let advertised = server.ls_remote("api", "writes").await?;
    assert!(advertised.contains("refs/tags/api-v1^{}"), "{advertised}");
    let clone = tempfile::tempdir()?;
    git(
        &[
            "clone",
            "-q",
            &server.repo_url("api", "writes"),
            clone.path().to_str().unwrap(),
        ],
        clone.path().parent().unwrap(),
    )?;
    assert!(git_in(clone.path(), &["tag", "-n1", "api-v1"])?.contains("release from API"));

    // Both archive formats are byte-for-byte `git archive`, extract cleanly,
    // and implement immutable validators plus byte ranges.
    for format in ["tar.gz", "zip"] {
        let expected = Command::new("git")
            .current_dir(&*src)
            .args([
                "archive",
                &format!("--format={format}"),
                "--prefix=snapshot/",
                &head,
            ])
            .output()?;
        assert!(expected.status.success());
        let response = client
            .get(format!("{api}/archive/main"))
            .query(&[("format", format), ("prefix", "snapshot/")])
            .send()
            .await?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let headers = response.headers().clone();
        let etag = headers[reqwest::header::ETAG].to_str()?.to_string();
        assert!(
            headers[reqwest::header::CACHE_CONTROL]
                .to_str()?
                .contains("immutable")
        );
        assert_eq!(headers[reqwest::header::ACCEPT_RANGES], "bytes");
        let archive = response.bytes().await?;
        assert_eq!(archive.as_ref(), expected.stdout.as_slice());

        let files = tempfile::tempdir()?;
        let archive_path = files.path().join(format!("archive.{format}"));
        std::fs::write(&archive_path, &archive)?;
        let extract = files.path().join("extract");
        std::fs::create_dir(&extract)?;
        let status = if format == "tar.gz" {
            Command::new("tar")
                .args(["-xzf", archive_path.to_str().unwrap(), "-C"])
                .arg(&extract)
                .status()?
        } else {
            Command::new("unzip")
                .args(["-q", archive_path.to_str().unwrap(), "-d"])
                .arg(&extract)
                .status()?
        };
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(extract.join("snapshot/f1_0.txt"))?,
            "content 1\n"
        );

        let not_modified = client
            .get(format!("{api}/archive/main"))
            .query(&[("format", format), ("prefix", "snapshot/")])
            .header(reqwest::header::IF_NONE_MATCH, &etag)
            .send()
            .await?;
        assert_eq!(not_modified.status(), reqwest::StatusCode::NOT_MODIFIED);
        let partial = client
            .get(format!("{api}/archive/main"))
            .query(&[("format", format), ("prefix", "snapshot/")])
            .header(reqwest::header::RANGE, "bytes=0-31")
            .header(reqwest::header::IF_RANGE, &etag)
            .send()
            .await?;
        assert_eq!(partial.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            partial.headers()[reqwest::header::CONTENT_RANGE],
            format!("bytes 0-31/{}", archive.len())
        );
        assert_eq!(partial.bytes().await?.as_ref(), &archive[..32]);
    }
    assert_eq!(
        archive_cache_count(&server).await?,
        0,
        "free-form prefix variants must never enter the bucket cache"
    );

    let expected = Command::new("git")
        .current_dir(&*src)
        .args(["archive", "--format=tar.gz", &head])
        .output()?;
    assert!(expected.status.success());
    let unprefixed = client.get(format!("{api}/archive/main")).send().await?;
    assert_eq!(unprefixed.status(), reqwest::StatusCode::OK);
    assert_eq!(
        unprefixed.bytes().await?.as_ref(),
        expected.stdout.as_slice()
    );
    assert_eq!(
        archive_cache_count(&server).await?,
        1,
        "only the bounded prefix-free commit/format variant is cached"
    );

    let sibling = server.start_sibling_with(|_| {}).await?;
    let cached = client
        .get(format!("{}/api/writes/api/archive/main", sibling.base_url))
        .send()
        .await?;
    assert_eq!(cached.status(), reqwest::StatusCode::OK);
    let tasks: serde_json::Value =
        serde_json::from_str(&sibling.get_text("/api/writes/api/tasks", &[]).await?)?;
    assert!(
        !tasks["recent"]
            .as_array()
            .is_some_and(|records| records.iter().any(|record| record["kind"] == "archive")),
        "a sibling must use the shared archive cache without regenerating it: {tasks}"
    );

    assert_eq!(
        client
            .delete(format!("{api}/refs/tags/light"))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert_eq!(
        client
            .delete(format!("{api}/refs/tags/api-v1"))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_api_batch_commits_and_merges() -> TestResult {
    let server = Server::start_with_tweak(|config| {
        config.git.max_commit_changes = 3;
        config.git.max_commit_bytes = gitcask_config::ByteSize::b(12);
    })
    .await?;
    server.put_repo("api", "objects").await?;
    let source = tempfile::tempdir()?;
    git_in(source.path(), &["init", "-q", "-b", "main"])?;
    git_in(source.path(), &["config", "user.name", "API Tester"])?;
    git_in(source.path(), &["config", "user.email", "api@example.test"])?;
    std::fs::write(source.path().join("modify.txt"), "before\n")?;
    std::fs::write(source.path().join("delete.txt"), "remove me\n")?;
    std::fs::write(source.path().join("shared.txt"), "root\n")?;
    git_in(source.path(), &["add", "."])?;
    git_in(source.path(), &["commit", "-q", "-m", "root"])?;
    let root = git_in(source.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    git_in(
        source.path(),
        &[
            "push",
            "-q",
            &server.repo_url("api", "objects"),
            "main:main",
        ],
    )?;

    let client = reqwest::Client::new();
    let api = format!("{}/api/objects/api", server.base_url);
    let identity = serde_json::json!({
        "name": "API Committer",
        "email": "committer@example.test",
        "when": "2026-09-01T12:34:56+09:00"
    });
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let response = client
        .post(format!("{api}/commits"))
        .json(&serde_json::json!({
            "branch": "main",
            "message": "three file batch",
            "expected_head_oid": &root,
            "committer": &identity,
            "changes": [
                {"op": "upsert", "path": "nested/added.txt", "content": encode(b"added\n"), "mode": "100644"},
                {"op": "upsert", "path": "modify.txt", "content": encode(b"after\n"), "mode": "100755"},
                {"op": "delete", "path": "delete.txt"}
            ]
        }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let commit: serde_json::Value = response.json().await?;
    assert_eq!(commit["ref"], "refs/heads/main");
    assert_eq!(commit["oid"], commit["commit_oid"]);
    assert!(commit["seq"].as_u64().unwrap() > 0);
    let committed = commit["commit_oid"].as_str().unwrap().to_string();

    let checkout = tempfile::tempdir()?;
    git(
        &[
            "clone",
            "-q",
            &server.repo_url("api", "objects"),
            checkout.path().to_str().unwrap(),
        ],
        checkout.path().parent().unwrap(),
    )?;
    assert_eq!(
        std::fs::read_to_string(checkout.path().join("nested/added.txt"))?,
        "added\n"
    );
    assert_eq!(
        std::fs::read_to_string(checkout.path().join("modify.txt"))?,
        "after\n"
    );
    assert!(!checkout.path().join("delete.txt").exists());
    assert_eq!(
        git_in(checkout.path(), &["rev-list", "--count", "HEAD"])?.trim(),
        "2"
    );
    assert_eq!(
        git_in(
            checkout.path(),
            &["show", "-s", "--format=%an <%ae>", "HEAD"]
        )?
        .trim(),
        "API Committer <committer@example.test>",
        "author defaults to the committer"
    );

    let stale = client
        .post(format!("{api}/commits"))
        .json(&serde_json::json!({
            "branch": "main",
            "message": "stale",
            "expected_head_oid": &root,
            "committer": &identity,
            "changes": [{"op": "upsert", "path": "stale.txt", "content": encode(b"stale\n"), "mode": "100644"}]
        }))
        .send()
        .await?;
    assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
    let empty = client
        .post(format!("{api}/commits"))
        .json(&serde_json::json!({
            "branch": "main",
            "message": "same tree",
            "expected_head_oid": &committed,
            "committer": &identity,
            "changes": [{"op": "upsert", "path": "modify.txt", "content": encode(b"after\n"), "mode": "100755"}]
        }))
        .send()
        .await?;
    assert_eq!(empty.status(), reqwest::StatusCode::BAD_REQUEST);
    let oversized = client
        .post(format!("{api}/commits"))
        .json(&serde_json::json!({
            "branch": "main",
            "message": "too large",
            "expected_head_oid": &committed,
            "committer": &identity,
            "changes": [{"op": "upsert", "path": "large.txt", "content": encode(b"thirteen byte!"), "mode": "100644"}]
        }))
        .send()
        .await?;
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    let too_many = client
        .post(format!("{api}/commits"))
        .json(&serde_json::json!({
            "branch": "main",
            "message": "too many",
            "expected_head_oid": &committed,
            "committer": &identity,
            "changes": [
                {"op": "delete", "path": "one"},
                {"op": "delete", "path": "two"},
                {"op": "delete", "path": "three"},
                {"op": "delete", "path": "four"}
            ]
        }))
        .send()
        .await?;
    assert_eq!(too_many.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    let rename = client
        .post(format!("{api}/commits"))
        .json(&serde_json::json!({
            "branch": "main",
            "message": "rename through API",
            "expected_head_oid": &committed,
            "committer": &identity,
            "changes": [{"op": "rename", "from": "nested/added.txt", "to": "moved.txt"}]
        }))
        .send()
        .await?;
    assert_eq!(rename.status(), reqwest::StatusCode::CREATED);
    git_in(checkout.path(), &["pull", "-q"])?;
    assert!(!checkout.path().join("nested/added.txt").exists());
    assert_eq!(
        std::fs::read_to_string(checkout.path().join("moved.txt"))?,
        "added\n"
    );

    // Build independent branch pairs locally, then exercise a true merge,
    // conflicts, both fast-forward outcomes, squash, and the no-write replay.
    git_in(source.path(), &["checkout", "-q", "-b", "feature", &root])?;
    std::fs::write(source.path().join("feature.txt"), "feature\n")?;
    git_in(source.path(), &["add", "feature.txt"])?;
    git_in(source.path(), &["commit", "-q", "-m", "feature"])?;
    let feature = git_in(source.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    git_in(
        source.path(),
        &["checkout", "-q", "-b", "merge-base", &root],
    )?;
    std::fs::write(source.path().join("base.txt"), "base\n")?;
    git_in(source.path(), &["add", "base.txt"])?;
    git_in(source.path(), &["commit", "-q", "-m", "base"])?;
    let merge_base = git_in(source.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    git_in(
        source.path(),
        &["checkout", "-q", "-b", "conflict-base", &root],
    )?;
    std::fs::write(source.path().join("shared.txt"), "base side\n")?;
    git_in(source.path(), &["commit", "-q", "-am", "conflict base"])?;
    let conflict_base = git_in(source.path(), &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    git_in(
        source.path(),
        &["checkout", "-q", "-b", "conflict-head", &root],
    )?;
    std::fs::write(source.path().join("shared.txt"), "head side\n")?;
    git_in(source.path(), &["commit", "-q", "-am", "conflict head"])?;
    git_in(source.path(), &["branch", "ff-base", &root])?;
    git_in(source.path(), &["branch", "squash-base", &root])?;
    git_in(
        source.path(),
        &[
            "push",
            "-q",
            &server.repo_url("api", "objects"),
            "feature:feature",
            "merge-base:merge-base",
            "conflict-base:conflict-base",
            "conflict-head:conflict-head",
            "ff-base:ff-base",
            "squash-base:squash-base",
        ],
    )?;

    let merge_response = client
        .post(format!("{api}/merges"))
        .json(&serde_json::json!({
            "base": "merge-base",
            "head": "feature",
            "message": "merge feature",
            "committer": &identity,
            "strategy": "merge",
            "expected_base_oid": &merge_base
        }))
        .send()
        .await?;
    assert_eq!(merge_response.status(), reqwest::StatusCode::CREATED);
    let merged: serde_json::Value = merge_response.json().await?;
    let merged_oid = merged["oid"].as_str().unwrap().to_string();
    assert_eq!(merged["commit_oid"], merged["oid"]);

    let merged_clone = tempfile::tempdir()?;
    git(
        &[
            "clone",
            "-q",
            "--branch",
            "merge-base",
            &server.repo_url("api", "objects"),
            merged_clone.path().to_str().unwrap(),
        ],
        merged_clone.path().parent().unwrap(),
    )?;
    assert_eq!(
        std::fs::read_to_string(merged_clone.path().join("base.txt"))?,
        "base\n"
    );
    assert_eq!(
        std::fs::read_to_string(merged_clone.path().join("feature.txt"))?,
        "feature\n"
    );
    assert_eq!(
        git_in(
            merged_clone.path(),
            &["rev-list", "--parents", "-n", "1", "HEAD"]
        )?
        .split_whitespace()
        .count(),
        3,
        "merge commit has two parents"
    );

    let already = client
        .post(format!("{api}/merges"))
        .json(&serde_json::json!({
            "base": "merge-base", "head": "feature", "message": "again",
            "committer": &identity, "strategy": "merge", "expected_base_oid": &merged_oid
        }))
        .send()
        .await?;
    assert_eq!(already.status(), reqwest::StatusCode::OK);
    let already: serde_json::Value = already.json().await?;
    assert_eq!(already["already_merged"], true);
    assert_eq!(already["seq"], 0);

    let ff_fail = client
        .post(format!("{api}/merges"))
        .json(&serde_json::json!({
            "base": "merge-base", "head": "conflict-head", "message": "ff only",
            "committer": &identity, "strategy": "fast-forward-only", "expected_base_oid": &merged_oid
        }))
        .send()
        .await?;
    assert_eq!(ff_fail.status(), reqwest::StatusCode::CONFLICT);

    let conflict = client
        .post(format!("{api}/merges"))
        .json(&serde_json::json!({
            "base": "conflict-base", "head": "conflict-head", "message": "conflict",
            "committer": &identity, "strategy": "merge", "expected_base_oid": &conflict_base
        }))
        .send()
        .await?;
    assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);
    let conflict: serde_json::Value = conflict.json().await?;
    assert_eq!(conflict["error"], "merge_conflict");
    assert!(
        conflict["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path == "shared.txt")
    );

    let ff = client
        .post(format!("{api}/merges"))
        .json(&serde_json::json!({
            "base": "ff-base", "head": "feature", "message": "ff",
            "committer": &identity, "strategy": "fast-forward-only", "expected_base_oid": &root
        }))
        .send()
        .await?;
    assert_eq!(ff.status(), reqwest::StatusCode::CREATED);
    let ff: serde_json::Value = ff.json().await?;
    assert_eq!(ff["oid"], feature);
    assert!(ff.get("commit_oid").is_none());

    let squash = client
        .post(format!("{api}/merges"))
        .json(&serde_json::json!({
            "base": "squash-base", "head": "feature", "message": "squash",
            "committer": &identity, "strategy": "squash", "expected_base_oid": &root
        }))
        .send()
        .await?;
    assert_eq!(squash.status(), reqwest::StatusCode::CREATED);
    let squash: serde_json::Value = squash.json().await?;
    git_in(
        merged_clone.path(),
        &["fetch", "-q", "origin", "squash-base"],
    )?;
    assert_eq!(
        git_in(
            merged_clone.path(),
            &[
                "rev-list",
                "--parents",
                "-n",
                "1",
                squash["oid"].as_str().unwrap()
            ]
        )?
        .split_whitespace()
        .count(),
        2,
        "squash commit has only the base parent"
    );
    Ok(())
}

fn mint_jwt(private_key: &str, permission: &str, ttl_seconds: u64) -> anyhow::Result<String> {
    gitcask_server::auth::mint_token(
        private_key,
        "https://issuer.e2e",
        Some("gitcask-e2e"),
        &format!("e2e:{permission}"),
        &[format!("jwt/r:{permission}")],
        Duration::from_secs(ttl_seconds),
    )
}

fn jwt_repo_url(server: &str, token: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(&format!("{server}/jwt/r.git"))?;
    url.set_username("ignored")
        .map_err(|()| anyhow::anyhow!("cannot set gate URL username"))?;
    url.set_password(Some(token))
        .map_err(|()| anyhow::anyhow!("cannot set gate URL password"))?;
    Ok(url.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jwt_push_clone_scope_expiry_spoofing_signature_and_lfs() -> TestResult {
    let (private_key, public_key) = gitcask_server::auth::generate_key_pair_pem()?;
    let (wrong_private_key, _) = gitcask_server::auth::generate_key_pair_pem()?;
    let server = Server::start_with_tweak(move |config| {
        config.server.auth_mode = gitcask_config::AuthMode::Jwt;
        config.auth.jwt.public_key = Some(public_key);
        config.auth.jwt.issuer = "https://issuer.e2e".into();
        config.auth.jwt.audience = Some("gitcask-e2e".into());
        config.auth.jwt.leeway = Duration::ZERO;
    })
    .await?;

    let client = reqwest::Client::new();
    let admin = mint_jwt(&private_key, "admin", 60)?;
    let read = mint_jwt(&private_key, "read", 60)?;
    let expired = mint_jwt(&private_key, "read", 1)?;
    let wrong_signature = mint_jwt(&wrong_private_key, "read", 60)?;
    let create = client
        .put(format!("{}/jwt/r.git", server.base_url))
        .bearer_auth(&admin)
        .send()
        .await?;
    anyhow::ensure!(create.status().is_success(), "create: {}", create.status());

    let admin_url = jwt_repo_url(&server.base_url, &admin)?;
    let src = TestRepo::synthetic(2, 2)?;
    git_in(&src, &["remote", "add", "origin", &admin_url])?;
    git_in(&src, &["push", "-u", "origin", "main"])?;
    let clone_dir = tempfile::tempdir()?;
    git(
        &[
            "clone",
            &admin_url,
            clone_dir
                .path()
                .join("clone")
                .to_str()
                .context("clone path")?,
        ],
        clone_dir.path(),
    )?;
    assert_eq!(
        git_in(&src, &["rev-parse", "main"])?.trim(),
        git_in(&clone_dir.path().join("clone"), &["rev-parse", "main"])?.trim()
    );

    let denied = client
        .get(format!(
            "{}/jwt/r.git/info/refs?service=git-receive-pack",
            server.base_url
        ))
        .bearer_auth(&read)
        .send()
        .await?;
    assert_eq!(denied.status(), axum::http::StatusCode::NOT_FOUND);

    let read_api = client
        .get(format!("{}/jwt/r/api/refs", server.base_url))
        .bearer_auth(&read)
        .send()
        .await?;
    assert_eq!(read_api.status(), axum::http::StatusCode::OK);
    let denied_api_write = client
        .put(format!("{}/jwt/r/api/refs/heads/denied", server.base_url))
        .bearer_auth(&read)
        .json(&serde_json::json!({"target": "main"}))
        .send()
        .await?;
    assert_eq!(denied_api_write.status(), axum::http::StatusCode::NOT_FOUND);

    let lfs_object = serde_json::json!({
        "oid": "0000000000000000000000000000000000000000000000000000000000000000",
        "size": 0
    });
    let denied_lfs_upload = client
        .post(format!(
            "{}/jwt/r.git/info/lfs/objects/batch",
            server.base_url
        ))
        .bearer_auth(&read)
        .json(&serde_json::json!({"operation": "upload", "objects": [&lfs_object]}))
        .send()
        .await?;
    assert_eq!(
        denied_lfs_upload.status(),
        axum::http::StatusCode::NOT_FOUND
    );
    let allowed_lfs_download = client
        .post(format!(
            "{}/jwt/r.git/info/lfs/objects/batch",
            server.base_url
        ))
        .bearer_auth(&read)
        .json(&serde_json::json!({"operation": "download", "objects": [&lfs_object]}))
        .send()
        .await?;
    assert_eq!(allowed_lfs_download.status(), axum::http::StatusCode::OK);

    let spoofed = client
        .delete(format!("{}/jwt/r.git", server.base_url))
        .bearer_auth(&read)
        .header("X-Gitcask-Principal", "spoofed")
        .header("X-Gitcask-Write", "1")
        .header("X-Gitcask-Admin", "1")
        .send()
        .await?;
    assert_eq!(spoofed.status(), axum::http::StatusCode::NOT_FOUND);

    let bad_signature = client
        .get(format!(
            "{}/jwt/r.git/info/refs?service=git-upload-pack",
            server.base_url
        ))
        .bearer_auth(&wrong_signature)
        .send()
        .await?;
    assert_eq!(bad_signature.status(), axum::http::StatusCode::UNAUTHORIZED);

    tokio::time::sleep(Duration::from_secs(2)).await;
    let expired_response = client
        .get(format!(
            "{}/jwt/r.git/info/refs?service=git-upload-pack",
            server.base_url
        ))
        .bearer_auth(&expired)
        .send()
        .await?;
    assert_eq!(
        expired_response.status(),
        axum::http::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        expired_response
            .headers()
            .get(axum::http::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok()),
        Some(r#"Basic realm="gitcask""#)
    );

    if git_lfs_present() {
        git_in(&src, &["lfs", "install", "--local"])?;
        git_in(&src, &["lfs", "track", "*.bin"])?;
        std::fs::write(src.join("jwt.bin"), b"LFS through gitcask JWT\n")?;
        git_in(&src, &["add", ".gitattributes", "jwt.bin"])?;
        git_in(&src, &["commit", "-m", "jwt lfs"])?;
        git_in(&src, &["push", "origin", "main"])?;
        let lfs_clone = tempfile::tempdir()?;
        git(
            &[
                "clone",
                &admin_url,
                lfs_clone
                    .path()
                    .join("clone")
                    .to_str()
                    .context("LFS clone path")?,
            ],
            lfs_clone.path(),
        )?;
        assert_eq!(
            std::fs::read(lfs_clone.path().join("clone/jwt.bin"))?,
            b"LFS through gitcask JWT\n"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_refs_v2_advertises_capabilities() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;
    let out = server
        .get_text(
            "/t/r.git/info/refs?service=git-upload-pack",
            &[("Git-Protocol", "version=2")],
        )
        .await?;
    assert!(out.contains("# service=git-upload-pack"));
    assert!(out.contains("version 2"));
    assert!(out.contains("ls-refs=unborn"));
    assert!(out.contains("fetch=shallow wait-for-done"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_clone_roundtrip_v2() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(3, 4)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "first"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    let push_started = Instant::now();
    git_in(&src, &["push", "-u", "origin", "main"])?;
    if std::env::var_os("GITCASK_TEST_PRINT_PUSH_TIMING").is_some() {
        println!("small push took {:?}", push_started.elapsed());
    }

    let clone_dir = tempfile::tempdir()?;
    git(
        &[
            "clone",
            &server.repo_url("t", "r"),
            clone_dir.path().to_str().unwrap(),
        ],
        clone_dir.path().parent().unwrap(),
    )?;

    // fsck + refs equal
    git_in(clone_dir.path(), &["fsck"])?;
    let src_head = git_in(&src, &["rev-parse", "main"])?;
    let cl_head = git_in(clone_dir.path(), &["rev-parse", "main"])?;
    assert_eq!(src_head.trim(), cl_head.trim());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_cache_is_evicted_and_rematerialized() -> TestResult {
    let _install_delay_env = INSTALL_DELAY_ENV_LOCK.lock().await;
    let server = Server::start_with_tweak(|c| {
        c.cache.evict_idle_after = Duration::from_secs(1);
        c.cache.evict_interval = Duration::from_secs(1);
    })
    .await?;
    server.put_repo("t", "evict").await?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "evict"])?;
    git_in(
        &src,
        &[
            "push",
            &server.repo_url("t", "evict"),
            "HEAD:refs/heads/main",
        ],
    )?;

    let first = tempfile::tempdir()?;
    git(
        &[
            "clone",
            &server.repo_url("t", "evict"),
            first.path().join("clone").to_str().unwrap(),
        ],
        first.path(),
    )
    .context("first clone before eviction")?;

    let cached_repo = server.state.cfg.cache.dir.join("t/evict.git");
    assert!(cached_repo.exists(), "clone did not materialize the cache");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        !cached_repo.exists(),
        "idle repository cache was not evicted"
    );

    let second = tempfile::tempdir()?;
    git(
        &[
            "clone",
            &server.repo_url("t", "evict"),
            second.path().join("clone").to_str().unwrap(),
        ],
        second.path(),
    )
    .context("clone after eviction")?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clone_protocol_v0() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(2, 2)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "v0"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    let clone_dir = tempfile::tempdir()?;
    git(
        &[
            "-c",
            "protocol.version=0",
            "clone",
            &server.repo_url("t", "r"),
            clone_dir.path().to_str().unwrap(),
        ],
        clone_dir.path().parent().unwrap(),
    )?;
    git_in(clone_dir.path(), &["fsck"])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn second_push_and_fetch() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    git_in(&src, &["commit", "--allow-empty", "-m", "b"])?;
    git_in(&src, &["push", "origin", "main"])?;

    let clone_dir = tempfile::tempdir()?;
    git(
        &[
            "clone",
            &server.repo_url("t", "r"),
            clone_dir.path().to_str().unwrap(),
        ],
        clone_dir.path().parent().unwrap(),
    )?;
    let h = git_in(clone_dir.path(), &["rev-parse", "main"])?;
    let src_h = git_in(&src, &["rev-parse", "main"])?;
    assert_eq!(h.trim(), src_h.trim());

    // fetch after a third push
    git_in(&src, &["commit", "--allow-empty", "-m", "c"])?;
    git_in(&src, &["push", "origin", "main"])?;
    git_in(clone_dir.path(), &["fetch", "origin"])?;
    let h2 = git_in(clone_dir.path(), &["rev-parse", "origin/main"])?;
    let src_h2 = git_in(&src, &["rev-parse", "main"])?;
    assert_eq!(h2.trim(), src_h2.trim());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_fast_forward_rejected() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    // Divergent history: reset to a different root.
    let other = TestRepo::synthetic(1, 1)?;
    git_in(&other, &["commit", "--allow-empty", "-m", "other"])?;
    git_in(&other, &["branch", "-M", "main"])?;
    git_in(
        &other,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    let res = Command::new("git")
        .current_dir(&*other)
        .args(["push", "origin", "main"])
        .output()?;
    let stderr = String::from_utf8_lossy(&res.stderr);
    assert!(
        !res.status.success(),
        "non-ff push should be rejected; stderr: {stderr}",
    );
    assert!(
        stderr.contains("non-fast-forward")
            || stderr.contains("! [remote rejected]")
            || stderr.contains("ng"),
        "stderr should mention rejection"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_delete_ref() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(&src, &["branch", "topic"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main", "topic"])?;

    git_in(&src, &["push", "origin", "--delete", "topic"])?;

    let refs = server.ls_remote("t", "r").await?;
    assert!(!refs.contains("refs/heads/topic"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dangling_head_clone_and_ls_remote() -> TestResult {
    // The repo's HEAD points at a branch that was never pushed (only `other`
    // exists). ls-refs must not emit an empty oid for HEAD; clone must work.
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(
        &src,
        &["push", &server.repo_url("t", "r"), "HEAD:refs/heads/other"],
    )?;

    let refs = server.ls_remote("t", "r").await?;
    assert!(refs.contains("refs/heads/other"));
    for line in refs.lines() {
        assert!(
            !line.starts_with(' '),
            "empty oid in ls-remote line: {line:?}"
        );
    }

    let dst = tempfile::tempdir()?;
    let out = std::process::Command::new("git")
        .args([
            "clone",
            "-q",
            &server.repo_url("t", "r"),
            dst.path().join("c").to_str().unwrap(),
        ])
        .output()?;
    assert!(
        out.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_create_delete() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(
        &src,
        &["push", &server.repo_url("t", "r"), "HEAD:refs/heads/main"],
    )?;

    let client = reqwest::Client::new();
    let del = client
        .delete(format!("{}/t/r.git", server.base_url))
        .send()
        .await?;
    assert!(
        del.status() == 204 || del.status() == 200,
        "delete -> {}",
        del.status()
    );
    assert_eq!(
        server
            .get_status("/t/r.git/info/refs?service=git-upload-pack")
            .await?,
        axum::http::StatusCode::NOT_FOUND
    );
    let del_again = client
        .delete(format!("{}/t/r.git", server.base_url))
        .send()
        .await?;
    assert_eq!(del_again.status(), 404);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_clones_and_pushes_with_telemetry() -> TestResult {
    // Regression: tracing spans entered across .await under a multi-threaded
    // runtime corrupted the span registry ("tried to clone a span that already
    // closed") and aborted the process on a serverless host under load.
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;
    let src = TestRepo::synthetic(20, 3)?;
    git_in(
        &src,
        &["push", &server.repo_url("t", "r"), "HEAD:refs/heads/main"],
    )?;

    let url = server.repo_url("t", "r");
    let mut tasks = Vec::new();
    for i in 0..48 {
        let url = url.clone();
        tasks.push(tokio::task::spawn_blocking(
            move || -> anyhow::Result<()> {
                let dir = tempfile::tempdir()?;
                let dst = dir.path().join("c");
                let out = std::process::Command::new("git")
                    .args(["clone", "-q", &url, dst.to_str().unwrap()])
                    .output()?;
                anyhow::ensure!(
                    out.status.success(),
                    "clone {i}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                Ok(())
            },
        ));
    }
    for i in 0..16 {
        let url = url.clone();
        tasks.push(tokio::task::spawn_blocking(
            move || -> anyhow::Result<()> {
                let dir = tempfile::tempdir()?;
                let dst = dir.path().join("w");
                let out = std::process::Command::new("git")
                    .args(["clone", "-q", &url, dst.to_str().unwrap()])
                    .output()?;
                anyhow::ensure!(
                    out.status.success(),
                    "push-clone {i}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                std::fs::write(dst.join(format!("f{i}.txt")), format!("{i}\n"))?;
                git_in(&dst, &["add", "."])?;
                git_in(
                    &dst,
                    &[
                        "-c",
                        "user.email=t@t",
                        "-c",
                        "user.name=t",
                        "commit",
                        "-q",
                        "-m",
                        "x",
                    ],
                )?;
                let out = std::process::Command::new("git")
                    .current_dir(&dst)
                    .args(["push", "-q", "origin", &format!("HEAD:refs/heads/w{i}")])
                    .output()?;
                anyhow::ensure!(
                    out.status.success(),
                    "push {i}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                Ok(())
            },
        ));
    }
    let mut failures = Vec::new();
    for t in tasks {
        if let Err(e) = t.await? {
            failures.push(e.to_string());
        }
    }
    assert!(
        failures.is_empty(),
        "{} failures: {:?}",
        failures.len(),
        &failures[..failures.len().min(5)]
    );
    let refs = server.ls_remote("t", "r").await?;
    for i in 0..16 {
        assert!(refs.contains(&format!("refs/heads/w{i}")));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_tags() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &[
            "-c",
            "tag.forcesignannotated=false",
            "-c",
            "tag.gpgsign=false",
            "tag",
            "v1",
        ],
    )?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main", "--tags"])?;

    let refs = server.ls_remote("t", "r").await?;
    assert!(refs.contains("refs/tags/v1"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_clone_blob_none_and_lazy_fetch() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(3, 6)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "init"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    let clone_dir = tempfile::tempdir()?;
    git(
        &[
            "clone",
            "--filter=blob:none",
            &server.repo_url("t", "r"),
            clone_dir.path().to_str().unwrap(),
        ],
        clone_dir.path().parent().unwrap(),
    )?;
    // Lazy checkout: a file read triggers an on-demand fetch of the blob.
    git_in(clone_dir.path(), &["checkout", "main"])?;
    // List files (forces blob fetches for the worktree).
    git_in(clone_dir.path(), &["ls-files"])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ls_remote_lists_refs() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    let refs = server.ls_remote("t", "r").await?;
    assert!(refs.contains("refs/heads/main"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_instances_consistency() -> TestResult {
    // Two server processes sharing one MemoryStore, different cache dirs.
    let (a, b) = Server::start_pair().await?;
    a.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(2, 2)?;
    git_in(&src, &["commit", "--allow-empty", "-m", "a"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(&src, &["remote", "add", "origin", &a.repo_url("t", "r")])?;
    git_in(&src, &["push", "origin", "main"])?;

    // Immediate clone from B (the other instance) must see the push.
    let clone_dir = tempfile::tempdir()?;
    git(
        &[
            "clone",
            &b.repo_url("t", "r"),
            clone_dir.path().to_str().unwrap(),
        ],
        clone_dir.path().parent().unwrap(),
    )?;
    let h = git_in(clone_dir.path(), &["rev-parse", "main"])?;
    let src_h = git_in(&src, &["rev-parse", "main"])?;
    assert_eq!(h.trim(), src_h.trim());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_repo_clone_timing() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "big").await?;

    let synth_start = Instant::now();
    let src = TestRepo::synthetic(2000, 5)?;
    println!("2k synthetic repo took {:?}", synth_start.elapsed());
    git_in(&src, &["commit", "--allow-empty", "-m", "seed"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "big")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    let clone_dir = tempfile::tempdir()?;
    let start = std::time::Instant::now();
    git(
        &[
            "clone",
            &server.repo_url("t", "big"),
            clone_dir.path().to_str().unwrap(),
        ],
        clone_dir.path().parent().unwrap(),
    )?;
    let elapsed = start.elapsed();
    println!("2k-commit clone took {elapsed:?}");
    git_in(clone_dir.path(), &["fsck"])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shallow_clone_then_unshallow() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "shallow").await?;
    let src = TestRepo::synthetic(4, 2)?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "shallow")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    let clone_dir = tempfile::tempdir()?;
    git(
        &[
            "clone",
            "--branch",
            "main",
            "--depth",
            "1",
            &server.repo_url("t", "shallow"),
            clone_dir.path().to_str().unwrap(),
        ],
        clone_dir.path().parent().unwrap(),
    )?;
    assert_eq!(
        git_in(clone_dir.path(), &["rev-list", "--count", "HEAD"])?.trim(),
        "1"
    );
    git_in(clone_dir.path(), &["fetch", "--unshallow", "origin"])?;
    assert_eq!(
        git_in(clone_dir.path(), &["rev-list", "--count", "HEAD"])?.trim(),
        "4"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn atomic_push_rejects_all_refs_when_one_is_non_ff() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "atomic").await?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(&src, &["branch", "topic"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "atomic")],
    )?;
    git_in(&src, &["push", "origin", "main", "topic"])?;
    let initial_topic = git_in(&src, &["rev-parse", "topic"])?;

    // Advance main on the server, leaving the other clone's main stale.
    git_in(&src, &["commit", "--allow-empty", "-m", "server main"])?;
    let server_main = git_in(&src, &["rev-parse", "main"])?;
    git_in(&src, &["push", "origin", "main"])?;

    let other_dir = tempfile::tempdir()?;
    git(
        &[
            "clone",
            "--branch",
            "main",
            &server.repo_url("t", "atomic"),
            other_dir.path().to_str().unwrap(),
        ],
        other_dir.path().parent().unwrap(),
    )?;
    git_in(
        other_dir.path(),
        &["update-ref", "refs/heads/main", initial_topic.trim()],
    )?;
    git_in(
        other_dir.path(),
        &["checkout", "-q", "-b", "topic", "origin/topic"],
    )?;
    git_in(
        other_dir.path(),
        &["commit", "--allow-empty", "-m", "topic ff"],
    )?;
    let out = Command::new("git")
        .current_dir(other_dir.path())
        .args(["push", "--atomic", "origin", "main", "topic"])
        .output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "atomic non-ff push unexpectedly succeeded: {stderr}"
    );
    assert!(
        stderr.contains("main"),
        "report should mention main: {stderr}"
    );
    assert!(
        stderr.contains("topic"),
        "atomic report should mention topic: {stderr}"
    );

    let refs = server.ls_remote("t", "atomic").await?;
    assert!(refs.contains(&format!("{}\trefs/heads/main", server_main.trim())));
    assert!(refs.contains(&format!("{}\trefs/heads/topic", initial_topic.trim())));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_option_is_recorded_in_wal_log() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "push-option").await?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &[
            "remote",
            "add",
            "origin",
            &server.repo_url("t", "push-option"),
        ],
    )?;
    git_in(&src, &["push", "--push-option=foo", "origin", "main"])?;
    let entries = server.read_log("t", "push-option").await?;
    assert!(
        entries
            .iter()
            .any(|entry| entry.meta.get("push_options").map(String::as_str) == Some("foo")),
        "push option missing from WAL metadata: {entries:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_protocol_v0() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "push-v0").await?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "push-v0")],
    )?;
    git_in(
        &src,
        &["-c", "protocol.version=0", "push", "origin", "main"],
    )?;
    let refs = server.ls_remote("t", "push-v0").await?;
    assert!(refs.contains("refs/heads/main"));
    Ok(())
}

/// Many-ref advertisement: v0 and v2 prefix filtering stay fast. The fast tier
/// uses 2k refs (~2 s); the ignored bench pushes 20k (dominated by git's own
/// client-side `send-pack`, ~70 s — see AGENTS.md §7). `GITCASK_TEST_REFS=N`
/// overrides the count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_refs_advertisement_and_v2_prefix_are_fast() -> TestResult {
    let n: usize = std::env::var("GITCASK_TEST_REFS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    many_refs_impl(n).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "bench: 20k-ref mirror push (~70 s, git client-side send-pack)"]
async fn bench_20k_ref_advertisement() -> TestResult {
    many_refs_impl(20_000).await
}

async fn many_refs_impl(n: usize) -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "many-refs").await?;
    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["branch", "-M", "main"])?;
    let head = git_in(&src, &["rev-parse", "main"])?;
    let mut update = Command::new("git")
        .current_dir(&*src)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let stdin = update.stdin.as_mut().context("update-ref stdin")?;
        for i in 0..n {
            writeln!(stdin, "create refs/heads/ref-{i:05} {}", head.trim())?;
        }
    }
    let update_output = update.wait_with_output()?;
    assert!(
        update_output.status.success(),
        "update-ref failed: {}",
        String::from_utf8_lossy(&update_output.stderr)
    );
    git_in(
        &src,
        &[
            "remote",
            "add",
            "origin",
            &server.repo_url("t", "many-refs"),
        ],
    )?;
    let push_start = Instant::now();
    git_in(&src, &["push", "--mirror", "origin"])?;
    println!("{n}-ref mirror push took {:?}", push_start.elapsed());
    assert!(push_start.elapsed() < std::time::Duration::from_secs(240));
    let start = Instant::now();
    let output = Command::new("git")
        .args(["ls-remote", &server.repo_url("t", "many-refs")])
        .output()?;
    let elapsed = start.elapsed();
    assert!(output.status.success(), "large ls-remote failed");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "ls-remote took {elapsed:?}"
    );
    let all = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        all.lines()
            .filter(|line| line.contains("refs/heads/ref-"))
            .count(),
        n,
        "wrong large-ref count"
    );

    let start = Instant::now();
    let output = Command::new("git")
        .args([
            "-c",
            "protocol.version=2",
            "ls-remote",
            "--refs",
            "--heads",
            &server.repo_url("t", "many-refs"),
            // 100 refs share this prefix at every n >= 2000 (ref-NNN00..ref-NNN99).
            &format!("refs/heads/ref-{:03}*", (n / 100) - 1),
        ])
        .output()?;
    let elapsed_v2 = start.elapsed();
    assert!(output.status.success(), "v2 prefix ls-remote failed");
    assert!(
        elapsed_v2 < std::time::Duration::from_secs(2),
        "v2 ls-refs took {elapsed_v2:?}"
    );
    let prefixed = String::from_utf8_lossy(&output.stdout);
    assert_eq!(prefixed.lines().count(), 100, "wrong v2 prefix count");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sha256_repo_roundtrip() -> TestResult {
    if !git_supports_sha256() {
        eprintln!("git init --object-format=sha256 unsupported; skipping");
        return Ok(());
    }
    let server = Server::start().await?;
    let url = format!("{}/t/sha256?object_format=sha256", server.base_url);
    let response = reqwest::Client::new().put(url).send().await?;
    assert!(
        response.status().is_success(),
        "sha256 create failed: {}",
        response.status()
    );
    let src_dir = tempfile::tempdir()?;
    git_in(src_dir.path(), &["init", "-q", "--object-format=sha256"])?;
    git_in(src_dir.path(), &["config", "user.name", "sha256"])?;
    git_in(src_dir.path(), &["config", "user.email", "sha256@gitcask"])?;
    std::fs::write(src_dir.path().join("file"), b"sha256\n")?;
    git_in(src_dir.path(), &["add", "file"])?;
    git_in(src_dir.path(), &["commit", "-q", "-m", "sha256"])?;
    git_in(src_dir.path(), &["branch", "-M", "main"])?;
    git_in(
        src_dir.path(),
        &["remote", "add", "origin", &server.repo_url("t", "sha256")],
    )?;
    git_in(src_dir.path(), &["push", "origin", "main"])?;
    let clone_dir = tempfile::tempdir()?;
    let clone = Command::new("git")
        .args([
            "clone",
            "--branch",
            "main",
            &server.repo_url("t", "sha256"),
            clone_dir.path().to_str().unwrap(),
        ])
        .output()?;
    if !clone.status.success() {
        eprintln!(
            "sha256 push succeeded but clone is unsupported: {}",
            String::from_utf8_lossy(&clone.stderr)
        );
        return Ok(());
    }
    assert_eq!(
        git_in(clone_dir.path(), &["rev-parse", "--show-object-format"])?.trim(),
        "sha256"
    );
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lfs_roundtrip_when_available() -> TestResult {
    if !git_lfs_present() {
        eprintln!("git lfs not present; skipping LFS test");
        return Ok(());
    }
    let server = Server::start().await?;
    server.put_repo("t", "r").await?;

    let src = TestRepo::synthetic(1, 1)?;
    git_in(&src, &["lfs", "install", "--local"])?;
    git_in(&src, &["lfs", "track", "*.bin"])?;
    git_in(&src, &["add", ".gitattributes"])?;
    std::fs::write(src.join("blob.bin"), b"this is a large blob payload\n")?;
    git_in(&src, &["add", "blob.bin"])?;
    git_in(&src, &["commit", "-m", "lfs"])?;
    git_in(&src, &["branch", "-M", "main"])?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "r")],
    )?;
    git_in(&src, &["push", "origin", "main"])?;

    let clone_dir = tempfile::tempdir()?;
    git(
        &[
            "-c",
            "filter.lfs.clean=git-lfs clean -- %f",
            "-c",
            "filter.lfs.smudge=git-lfs smudge -- %f",
            "-c",
            "filter.lfs.process=git-lfs filter-process",
            "-c",
            "filter.lfs.required=true",
            "clone",
            &server.repo_url("t", "r"),
            clone_dir.path().to_str().unwrap(),
        ],
        clone_dir.path().parent().unwrap(),
    )?;
    let content = std::fs::read(clone_dir.path().join("blob.bin"))?;
    assert_eq!(content, b"this is a large blob payload\n");

    // a large push (2026-08-21): a SECOND clone that never downloaded the
    // LFS bytes (`GIT_LFS_SKIP_SMUDGE`: pointers only, as for an object deep
    // in history) pushes a new commit. git-lfs's pre-push batches every
    // pointer reachable from the push; the server HAS the object and must say
    // so with no `actions` at all — a verify-only answer made git-lfs try to
    // upload bytes it does not have: "object … missing locally and on remote".
    let pointer_clone = tempfile::tempdir()?;
    let mut cmd = std::process::Command::new("git");
    cmd.env("GIT_LFS_SKIP_SMUDGE", "1")
        .args([
            "clone",
            "-q",
            &server.repo_url("t", "r"),
            pointer_clone.path().to_str().unwrap(),
        ])
        .current_dir(pointer_clone.path().parent().unwrap());
    assert!(cmd.output()?.status.success());
    let p = pointer_clone.path();
    assert!(
        std::fs::read_to_string(p.join("blob.bin"))?.starts_with("version https://git-lfs"),
        "pointer only"
    );
    assert!(
        !p.join(".git/lfs/objects").exists()
            || std::fs::read_dir(p.join(".git/lfs/objects"))?
                .next()
                .is_none(),
        "no local LFS bytes"
    );
    git_in(p, &["lfs", "install", "--local"])?;
    git_in(p, &["config", "user.email", "t@t"])?;
    git_in(p, &["config", "user.name", "T"])?;
    // Move the pointer (rename) so the push's pre-push batches its oid, exactly
    // like the pointers of files touched by the pushed commits — bytes still absent.
    git_in(p, &["mv", "blob.bin", "moved.bin"])?;
    git_in(
        p,
        &[
            "commit",
            "-q",
            "-m",
            "move the LFS file (pointer only, no bytes here)",
        ],
    )?;
    let out = std::process::Command::new("git")
        .current_dir(p)
        .env("GIT_TRACE", "1")
        .args(["push", "origin", "main"])
        .output()?;
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "push from a pointer-only clone must succeed (server has the object):\n{err}"
    );
    assert!(!err.contains("missing locally and on remote"), "{err}");
    Ok(())
}
fn git_lfs_present() -> bool {
    Command::new("git")
        .args(["lfs", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_supports_sha256() -> bool {
    let Ok(dir) = tempfile::tempdir() else {
        return false;
    };
    Command::new("git")
        .args([
            "init",
            "-q",
            "--object-format=sha256",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_is_narrated_over_sideband() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "narrate").await?;
    let src = tempfile::tempdir()?;
    git(&["init", "-q", "-b", "main", "."], src.path())?;
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "one",
        ],
        src.path(),
    )?;
    git(
        &["push", "-q", &server.repo_url("t", "narrate"), "main"],
        src.path(),
    )?;
    let dst = tempfile::tempdir()?;
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "protocol.version=2",
            "clone",
            "--progress",
            &server.repo_url("t", "narrate"),
            dst.path().to_str().unwrap(),
        ])
        .output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("remote: * gitcask: t/narrate"),
        "narration missing:\n{stderr}"
    );
    assert!(stderr.contains("local copy ready"), "{stderr}");
    println!("{stderr}");
    // `no-progress` is honoured: git sends it for its own lazy promisor fetches (blobs during a
    // sparse checkout of a blobless clone) and for any non-tty fetch without --progress; those
    // must not be narrated, by design, not because the host is silent.
    let quiet = tempfile::tempdir()?;
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "protocol.version=2",
            "clone",
            "--no-progress",
            &server.repo_url("t", "narrate"),
            quiet.path().to_str().unwrap(),
        ])
        .output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        !stderr.contains("remote: *"),
        "no-progress fetch was narrated:\n{stderr}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn max_wants_refuses_the_blobless_checkout_storm_with_the_fix() -> TestResult {
    let server = Server::start_with_tweak(|c| c.git.max_wants = 5).await?;
    server.put_repo("t", "storm").await?;
    // 12 commits → 12 distinct blobs in HEAD's tree (the synthetic repo reuses one blob per revision).
    let src = TestRepo::synthetic(12, 3)?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "storm")],
    )?;
    git_in(&src, &["push", "-q", "origin", "main"])?;
    let url = server.repo_url("t", "storm");

    // Full checkout after a blobless clone: the lazy fetch wants 12 blobs > 5 → refused, fix named.
    let dir = tempfile::tempdir()?;
    let out = std::process::Command::new("git")
        .args([
            "clone",
            "--filter=blob:none",
            "--progress",
            &url,
            dir.path().to_str().unwrap(),
        ])
        .output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "the checkout's blob fetch must be refused:\n{stderr}"
    );
    assert!(
        stderr.contains("asks for 12 objects at once (this host's bound is 5)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("git clone --filter=blob:none --sparse"),
        "the fix is in the error:\n{stderr}"
    );
    // The initial (commit/tree) fetch narrated the heads-up on band 2 before anything went wrong.
    assert!(
        stderr.contains("remote: * blobless clone: without --sparse or --no-checkout"),
        "{stderr}"
    );
    assert!(
        stderr.contains("refuses requests above 5 objects"),
        "{stderr}"
    );

    // --no-checkout: the initial fetch wants one tip; nothing lazy follows. Blobs come on demand, few at a time.
    let dir = tempfile::tempdir()?;
    let out = std::process::Command::new("git")
        .args([
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-checkout",
            &url,
            dir.path().to_str().unwrap(),
        ])
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        git_in(dir.path(), &["rev-parse", "HEAD"])?.trim(),
        git_in(&src, &["rev-parse", "main"])?.trim()
    );
    // One file = one lazy blob (≤ 5): served.
    let one = git_in(dir.path(), &["ls-tree", "--name-only", "HEAD"])?
        .lines()
        .next()
        .unwrap()
        .to_string();
    git_in(dir.path(), &["checkout", "-q", "HEAD", "--", &one])?;
    assert!(dir.path().join(&one).exists());
    Ok(())
}
/// A push is narrated on band 2 (`remote: * …`) from the first moment — the
/// server never lets the connection go silent while it syncs/unpacks — and
/// still ends with a clean report-status.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_is_narrated_over_sideband() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "pushnarr").await?;
    let src = tempfile::tempdir()?;
    git(&["init", "-q", "-b", "main", "."], src.path())?;
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "one",
        ],
        src.path(),
    )?;
    let out = std::process::Command::new("git")
        .args([
            "push",
            "--progress",
            &server.repo_url("t", "pushnarr"),
            "main",
        ])
        .current_dir(src.path())
        .output()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("remote: * gitcask: t/pushnarr"),
        "narration missing:\n{stderr}"
    );
    assert!(
        stderr.contains("[new branch]") || stderr.contains("main -> main"),
        "{stderr}"
    );
    Ok(())
}

/// A push from a shallow (`--depth=1`) clone sends `shallow <oid>` lines
/// before the commands; the server accepts it (prod: a large repository push → 500
/// "missing ref name").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_from_a_shallow_clone() -> TestResult {
    let server = Server::start().await?;
    server.put_repo("t", "shallowpush").await?;
    let src = TestRepo::synthetic(4, 2)?;
    git_in(
        &src,
        &[
            "remote",
            "add",
            "origin",
            &server.repo_url("t", "shallowpush"),
        ],
    )?;
    git_in(&src, &["push", "-q", "origin", "main"])?;
    let clone = tempfile::tempdir()?;
    git(
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &server.repo_url("t", "shallowpush"),
            clone.path().to_str().unwrap(),
        ],
        clone.path().parent().unwrap(),
    )?;
    assert!(clone.path().join(".git/shallow").exists());
    std::fs::write(clone.path().join("from-shallow.txt"), "hi\n")?;
    git_in(clone.path(), &["add", "from-shallow.txt"])?;
    git_in(
        clone.path(),
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "-q",
            "-m",
            "from a shallow clone",
        ],
    )?;
    let out = std::process::Command::new("git")
        .args(["push", "origin", "HEAD:refs/heads/from-shallow"])
        .current_dir(clone.path())
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let refs = server.ls_remote("t", "shallowpush").await?;
    assert!(refs.contains("refs/heads/from-shallow"), "{refs}");
    Ok(())
}

/// `--filter=tree:0` partial clone over the wire: commits only, then a
/// checkout lazily fetches trees and blobs (wants that are not commits, with
/// `allow-any-sha1-in-want`), plus `--depth` + filter together.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_clone_tree_zero_and_depth_with_filter() -> TestResult {
    let server = Server::start_with_tweak(|c| c.git.allow_any_sha1_in_want = true).await?;
    server.put_repo("t", "tree0").await?;
    let src = TestRepo::synthetic(5, 4)?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "tree0")],
    )?;
    git_in(&src, &["push", "-q", "origin", "main"])?;
    let head = git_in(&src, &["rev-parse", "main"])?;

    let clone = tempfile::tempdir()?;
    git(
        &[
            "clone",
            "-q",
            "--no-checkout",
            "--filter=tree:0",
            &server.repo_url("t", "tree0"),
            clone.path().to_str().unwrap(),
        ],
        clone.path().parent().unwrap(),
    )?;
    // Only commits came over: the root tree is not local yet.
    let has_tree = std::process::Command::new("git")
        .current_dir(clone.path())
        .args(["cat-file", "-e", &format!("{}^{{tree}}", head.trim())])
        .env("GIT_NO_LAZY_FETCH", "1")
        .status()?
        .success();
    assert!(!has_tree, "tree:0 clone must not contain trees");
    // Checkout fetches trees + blobs on demand.
    git_in(clone.path(), &["checkout", "-q", "main"])?;
    assert!(clone.path().join("f4_0.txt").exists());
    assert_eq!(
        git_in(clone.path(), &["rev-list", "--count", "HEAD"])?.trim(),
        "5"
    );

    let shallow = tempfile::tempdir()?;
    git(
        &[
            "clone",
            "-q",
            "--depth",
            "2",
            "--filter=blob:none",
            &server.repo_url("t", "tree0"),
            shallow.path().to_str().unwrap(),
        ],
        shallow.path().parent().unwrap(),
    )?;
    assert_eq!(
        git_in(shallow.path(), &["rev-list", "--count", "HEAD"])?.trim(),
        "2"
    );
    assert_eq!(
        std::fs::read_to_string(shallow.path().join("f4_0.txt"))?,
        "content 4\n"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn blocking_work_in_the_install_path_does_not_stall_requests() -> TestResult {
    let _install_delay_env = INSTALL_DELAY_ENV_LOCK.lock().await;
    // SAFETY: test process; read by the sibling's sync below.
    unsafe { std::env::set_var("GITCASK_TEST_BLOCK_INSTALL_MS", "2500") };
    let big = Server::start().await?;
    big.put_repo("t", "blk").await?;
    big.put_repo("t", "other2").await?;
    let src = TestRepo::synthetic(4, 2)?;
    git_in(
        &src,
        &["remote", "add", "origin", &big.repo_url("t", "blk")],
    )?;
    git_in(&src, &["push", "-q", "origin", "main"])?;
    let other = TestRepo::synthetic(1, 1)?;
    git_in(
        &other,
        &["remote", "add", "origin", &big.repo_url("t", "other2")],
    )?;
    git_in(&other, &["push", "-q", "origin", "main"])?;
    let small = big.start_sibling_with(|_| {}).await?;
    let sh = small
        .state
        .registry
        .open(&gitcask_git::RepoId::new("t", "blk")?)
        .await?;
    let install = tokio::spawn(async move {
        let t = std::time::Instant::now();
        let _g = sh.sync_full().await.unwrap();
        t.elapsed()
    });
    let mut worst = 0u128;
    let mut probes = 0;
    while !install.is_finished() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let t = std::time::Instant::now();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            small.get_text("/t/other2/api/refs", &[]),
        )
        .await;
        assert!(r.is_ok(), "refs timed out while the install path blocked");
        worst = worst.max(t.elapsed().as_millis());
        probes += 1;
    }
    unsafe { std::env::remove_var("GITCASK_TEST_BLOCK_INSTALL_MS") };
    let took = install.await?;
    assert!(took.as_millis() >= 2500, "{took:?}");
    assert!(probes >= 5, "runtime stalled: {probes} probes in {took:?}");
    assert!(
        worst < 1000,
        "a refs request took {worst} ms during the blocking install"
    );
    Ok(())
}

/// A refs-level request pulls the serving copy in the background when pack
/// prefetch is enabled, so the first object request finds it ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refs_level_requests_prefetch_pack_sets() -> TestResult {
    let big = Server::start().await?;
    big.put_repo("t", "prefetch").await?;
    let src = tempfile::tempdir()?;
    git(&["init", "-q", "-b", "main", "."], src.path())?;
    std::fs::write(src.path().join("f"), vec![b'x'; 200_000])?;
    git(&["add", "."], src.path())?;
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "-q",
            "-m",
            "one",
        ],
        src.path(),
    )?;
    git(
        &["push", "-q", &big.repo_url("t", "prefetch"), "main"],
        src.path(),
    )?;
    let id = gitcask_git::RepoId::new("t", "prefetch")?;

    let eager = big.start_sibling_with(|_| {}).await?;
    let _ = eager.ls_remote("t", "prefetch").await?;
    let h = eager.state.registry.open(&id).await?;
    for _ in 0..50 {
        if h.packs_ready() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(h.packs_ready(), "refs-level sync prefetched the pack set");
    Ok(())
}

/// `gitcask_http_inflight` counts a request until its response *body* is done — a streamed
/// fetch (sideband) and an SSE stream stay in flight past the handler's return — and is back
/// at 0 afterwards, so the watchdog's `inflight` field means what it says.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_inflight_gauge_covers_streamed_bodies_and_returns_to_zero() -> TestResult {
    let server = Server::start().await?;
    let http_inflight = || server.state.inflight.get();
    server.put_repo("t", "inflight").await?;
    let src = tempfile::tempdir()?;
    git(&["init", "-q", "-b", "main", "."], src.path())?;
    std::fs::write(src.path().join("f"), vec![b'y'; 300_000])?;
    git(&["add", "."], src.path())?;
    git(
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "-q",
            "-m",
            "one",
        ],
        src.path(),
    )?;
    git(
        &["push", "-q", &server.repo_url("t", "inflight"), "main"],
        src.path(),
    )?;
    let settle = || async {
        for _ in 0..50 {
            if http_inflight() == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    settle().await;
    assert_eq!(http_inflight(), 0, "idle server");

    // A streamed (sideband-narrated) clone: counted while streaming, 0 once git has the pack.
    let dst = tempfile::tempdir()?;
    git(
        &[
            "-c",
            "protocol.version=2",
            "clone",
            "-q",
            "--progress",
            &server.repo_url("t", "inflight"),
            dst.path().to_str().unwrap(),
        ],
        dst.path(),
    )?;
    settle().await;
    assert_eq!(http_inflight(), 0, "after a streamed fetch");

    // A request whose body never finishes (a stalled push upload) is in flight until the client
    // goes away; the count is taken at the middleware, before any handler work.
    let client = reqwest::Client::new();
    let never: futures::stream::Pending<Result<bytes::Bytes, std::io::Error>> =
        futures::stream::pending();
    let pending = tokio::spawn(
        client
            .post(format!(
                "{}/t/inflight.git/git-receive-pack",
                server.base_url
            ))
            .header("Content-Type", "application/x-git-receive-pack-request")
            .body(reqwest::Body::wrap_stream(never))
            .send(),
    );
    let mut saw_inflight = false;
    for _ in 0..100 {
        if http_inflight() >= 1 {
            saw_inflight = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        saw_inflight,
        "a request with an unfinished body counts as in flight"
    );
    pending.abort();
    settle().await;
    assert_eq!(http_inflight(), 0, "after the client went away");
    Ok(())
}

/// Read-your-writes on one instance, under contention: while N clients race pushes to one branch
/// and a reader spins on `ls-remote`, every read after a push's `ok` must show that push's tip —
/// the advertisement caches are keyed by the manifest version, and a publish that advertised the
/// new version before applying the refs locally let a reader cache the OLD refs under the NEW
/// version (reproduced roughly once in six rounds). 12 rounds × 6 pushers.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn reads_after_an_acknowledged_push_never_show_the_previous_tip() -> TestResult {
    // Widen the gap between the publish's two local-commit steps (refs applied; version advertised)
    // to 150 ms so the reader reliably lands in it: harmless in the right order, the poison window
    // in the wrong one.
    // SAFETY: test-only env var, read by the publish path of this process.
    unsafe { std::env::set_var("GITCASK_TEST_PUBLISH_GAP_MS", "150") };
    let server = Server::start().await?;
    server.put_repo("t", "ryw").await?;
    let src = TestRepo::synthetic(2, 1)?;
    git_in(
        &src,
        &["remote", "add", "origin", &server.repo_url("t", "ryw")],
    )?;
    git_in(&src, &["push", "-q", "origin", "main"])?;
    let url = server.repo_url("t", "ryw");
    let tip = |dir: &std::path::Path| -> String {
        let out = std::process::Command::new("git")
            .args(["ls-remote", &url, "refs/heads/main"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    };
    let mut stale = Vec::new();
    for round in 0..4 {
        let base = tip(&src);
        assert!(!base.is_empty());
        // 6 contenders from the same base, each with its own commit.
        let mut handles = Vec::new();
        for i in 0..6 {
            let d = tempfile::tempdir()?;
            git(
                &["clone", "-q", &url, d.path().to_str().unwrap()],
                d.path().parent().unwrap(),
            )?;
            std::fs::write(
                d.path().join(format!("r{round}-c{i}.txt")),
                format!("{round}/{i}\n"),
            )?;
            git_in(d.path(), &["add", "."])?;
            git_in(d.path(), &["commit", "-q", "-m", &format!("r{round} c{i}")])?;
            let sha = git_in(d.path(), &["rev-parse", "HEAD"])?.trim().to_string();
            let url2 = url.clone();
            let cwd = d.path().to_path_buf();
            handles.push((
                d,
                sha,
                std::thread::spawn(move || {
                    // true when this push won
                    let o = std::process::Command::new("git")
                        .current_dir(&cwd)
                        .args(["push", "-q", "--atomic", &url2, "HEAD:refs/heads/main"])
                        .output()
                        .unwrap();
                    if !o.status.success() {
                        eprintln!("push: {}", String::from_utf8_lossy(&o.stderr));
                    }
                    o.status.success()
                }),
            ));
        }
        // The reader: ls-remote continuously while the race runs.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (stop2, url3, srcp) = (stop.clone(), url.clone(), src.to_path_buf());
        let reader = std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                let out = std::process::Command::new("git")
                    .args(["ls-remote", &url3, "refs/heads/main"])
                    .current_dir(&srcp)
                    .output()
                    .unwrap();
                seen.push(
                    String::from_utf8_lossy(&out.stdout)
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string(),
                );
            }
            seen
        });
        let mut winner = None;
        for (_d, sha, h) in handles {
            if h.join().unwrap() {
                winner = Some(sha);
            }
        }
        let winner = winner.expect("exactly one push wins each round");
        // Read-your-writes: the first read after the last push returned must be the winner, and so
        // must every read after it.
        let mut after: Vec<String> = Vec::new();
        for _ in 0..5 {
            after.push(tip(&src));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let seen = reader.join().unwrap();
        for h in &after {
            if h != &winner {
                stale.push(format!("round {round}: read {h} after the winner {winner} was acknowledged (base {base})"));
            }
        }
        // The concurrent reader may see base or winner, never anything else, and never base again after winner.
        let mut saw_winner = false;
        for h in &seen {
            if h == &winner {
                saw_winner = true;
            } else if h == &base {
                if saw_winner {
                    stale.push(format!(
                        "round {round}: reader regressed to base after seeing the winner"
                    ));
                }
            } else if !h.is_empty() {
                stale.push(format!("round {round}: reader saw a foreign tip {h}"));
            }
        }
    }
    // SAFETY: see above.
    unsafe { std::env::remove_var("GITCASK_TEST_PUBLISH_GAP_MS") };
    assert!(stale.is_empty(), "stale reads:\n{}", stale.join("\n"));
    Ok(())
}
