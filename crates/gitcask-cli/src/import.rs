//! `gitcask import --from GITDIR OWNER/NAME` — import an existing git repo.
//!
//! Creates the repo in the store if it doesn't exist, then streams
//! `git pack-objects --all --stdout` output into `LocalRepo::ingest_pack`,
//! and publishes a ref snapshot from the source repo's refs.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tracing::info;

use gitcask_config::Config;
use gitcask_git::{IngestOptions, ObjectFormat};
use gitcask_store::open_store;
use gitcask_wal::Registry;

use crate::cli::parse_repo_id;

/// Which of the source's refs are published. No globs: `refs/heads/*` and
/// `refs/tags/*` (plus HEAD's target), never `refs/remotes/*`, `refs/pull/*`,
/// `refs/notes/*` or anything else — a mirror of a 466 k-ref monorepo must not
/// become 466 k WAL refs. With globs: exactly the refs matching one of them
/// (`*` matches any run of characters, including `/`; e.g. `refs/heads/main`,
/// `refs/tags/v*`). HEAD's target is always kept when it exists in the source.
pub struct RefFilter {
    globs: Vec<String>,
}

impl RefFilter {
    pub fn new(globs: Vec<String>) -> Self {
        RefFilter { globs }
    }
    pub fn keep(&self, name: &str, head_target: &str) -> bool {
        if !head_target.is_empty() && name == head_target {
            return true;
        }
        if self.globs.is_empty() {
            return name.starts_with("refs/heads/") || name.starts_with("refs/tags/");
        }
        self.globs.iter().any(|g| glob_match(g, name))
    }
}

/// Tiny glob: `*` matches any characters (including `/`), everything else literal.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !s.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == parts.len() - 1 {
            return s.len() >= pos && s[pos..].ends_with(part);
        } else if part.is_empty() {
            continue;
        } else {
            match s[pos..].find(part) {
                Some(at) => pos += at + part.len(),
                None => return false,
            }
        }
    }
    true
}

pub async fn run(
    from: PathBuf,
    repo: String,
    reuse_packs: bool,
    refs: Vec<String>,
    cfg: &Arc<Config>,
) -> Result<()> {
    // Open the store and create the repo.
    let store = open_store(cfg).await?;
    std::fs::create_dir_all(&cfg.cache.dir).ok();
    let registry = Registry::new(store, cfg.clone());

    run_with_registry(from, repo, reuse_packs, refs, cfg, &registry).await
}

pub(crate) async fn run_with_registry(
    from: PathBuf,
    repo: String,
    reuse_packs: bool,
    refs: Vec<String>,
    cfg: &Arc<Config>,
    registry: &Registry,
) -> Result<()> {
    let (owner, name) = parse_repo_id(&repo)?;
    let id = gitcask_git::RepoId::new(owner, name)?;

    // Resolve and inspect the source off the async worker. A bulk migration
    // can have several imports in flight, so even small synchronous fs reads
    // must not consume the runtime's serving threads.
    let (git_dir, format) = tokio::task::spawn_blocking(move || {
        let git_dir = resolve_git_dir(&from)?;
        let format = detect_object_format(&git_dir)?;
        Ok::<_, anyhow::Error>((git_dir, format))
    })
    .await
    .context("joining source repository inspection")??;
    info!(git_dir = %git_dir.display(), "importing from");

    info!(format = ?format, "object format");

    let handle = registry.open_or_create(&id, format).await?;
    // Ensure a newly-created repo is materialized before ingesting, then
    // release the read guard. Keeping it across publish would deadlock WAL's
    // writer path waiting for the local repository lock.
    let guard = handle.sync_full().await?;
    drop(guard);
    // Collect refs from the source repo.
    let refs_output = tokio::process::Command::new("git")
        .args(["for-each-ref", "--format=%(objectname) %(refname)"])
        .current_dir(&git_dir)
        .output()
        .await
        .context("running git for-each-ref")?;
    if !refs_output.status.success() {
        bail!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&refs_output.stderr)
        );
    }
    let refs_text = String::from_utf8_lossy(&refs_output.stdout);

    // HEAD target first: the filter always keeps it.
    let head_probe = tokio::process::Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .current_dir(&git_dir)
        .output()
        .await?;
    let head_for_filter = if head_probe.status.success() {
        String::from_utf8_lossy(&head_probe.stdout)
            .trim()
            .to_string()
    } else {
        String::new()
    };
    let filter = RefFilter::new(refs);

    // Build a RefTransaction from the source refs.
    let mut updates = Vec::new();
    let mut dropped = 0usize;
    for line in refs_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (oid, name) = line
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("bad for-each-ref line: {line}"))?;
        if !filter.keep(name, &head_for_filter) {
            dropped += 1;
            continue;
        }
        updates.push(gitcask_proto::v1::RefUpdate {
            name: name.to_string(),
            old_oid: "0".repeat(40),
            new_oid: oid.to_string(),
            new_symbolic_target: String::new(),
            new_peeled: String::new(),
        });
    }

    println!(
        "refs: publishing {} ({dropped} dropped by the ref filter)",
        updates.len()
    );
    // Add HEAD symbolic ref if it points to a branch.
    if !head_for_filter.is_empty() {
        updates.push(gitcask_proto::v1::RefUpdate {
            name: "HEAD".to_string(),
            old_oid: String::new(),
            new_oid: String::new(),
            new_symbolic_target: head_for_filter,
            new_peeled: String::new(),
        });
    }

    if reuse_packs {
        return import_reusing_packs(&handle, &id, &git_dir, updates).await;
    }

    // Stream `git pack-objects --all --stdout` into ingest_pack.
    // Do not pass `--revs`: that mode reads an explicit revision list from
    // stdin, while `--all` already enumerates every ref in the source repo.
    let pack_started = Instant::now();
    let mut pack_child = tokio::process::Command::new("git")
        .args(["pack-objects", "--all", "--stdout"])
        .current_dir(&git_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning git pack-objects")?;

    let stdout = pack_child
        .stdout
        .take()
        .context("no stdout from git pack-objects")?;

    // tokio::process::ChildStdout implements AsyncRead — stream directly into
    // index-pack rather than buffering a potentially multi-gigabyte pack.
    let local = handle.local();
    let ingested = local
        .ingest_pack(
            stdout,
            IngestOptions {
                fsck: cfg.wal.fsck_objects,
                max_bytes: Some(cfg.server.max_push_bytes.as_u64()),
                thin: false,
            },
        )
        .await?;

    // Wait for pack-objects to finish so failures cannot be hidden by a
    // successful index-pack (for example if the source ODB is corrupt).
    let status = pack_child
        .wait()
        .await
        .context("waiting for git pack-objects")?;
    if !status.success() {
        bail!("git pack-objects failed (exit {})", status);
    }
    let pack_elapsed_ms = pack_started.elapsed().as_secs_f64() * 1_000.0;

    if let Some(pack) = ingested {
        info!(
            checksum = %pack.checksum,
            objects = pack.object_count,
            pack_size = pack.pack_size,
            elapsed_ms = pack_elapsed_ms,
            "pack ingested"
        );
        println!(
            "pack: {} bytes, {} objects, {:.0} ms",
            pack.pack_size, pack.object_count, pack_elapsed_ms
        );

        // Publish the push with the ref transaction.
        let txn = gitcask_proto::v1::RefTransaction {
            updates,
            push_options: Vec::new(),
            atomic: true,
        };

        let result = handle
            .publish_push(
                Some(pack),
                txn,
                std::collections::HashMap::from([(
                    "imported_from".to_string(),
                    git_dir.display().to_string(),
                )]),
            )
            .await?;

        report_publish(&id, &result)?;
    } else {
        // Empty pack — repo has no objects, just publish ref updates.
        let txn = gitcask_proto::v1::RefTransaction {
            updates,
            push_options: Vec::new(),
            atomic: true,
        };
        let result = handle
            .publish_ref_update(txn, std::collections::HashMap::new())
            .await?;
        report_publish(&id, &result)?;
    }

    Ok(())
}

/// Resolve a path to a `.git` directory (supports working trees and bare repos).
pub(crate) fn resolve_git_dir(path: &std::path::Path) -> Result<PathBuf> {
    if path.join(".git").is_dir() {
        Ok(path.join(".git"))
    } else if path.join("HEAD").exists() && path.join("objects").is_dir() {
        // Looks like a bare repo or .git directory itself.
        Ok(path.to_path_buf())
    } else {
        bail!(
            "{} is not a git repository (no .git or bare layout)",
            path.display()
        );
    }
}

/// Detect the object format (sha1/sha256) from a git directory.
pub(crate) fn detect_object_format(git_dir: &std::path::Path) -> Result<ObjectFormat> {
    let config_path = git_dir.join("config");
    if config_path.exists() {
        let config = std::fs::read_to_string(&config_path)?;
        for line in config.lines() {
            let trimmed = line.trim();
            if let Some(val) = trimmed.strip_prefix("objectformat = ") {
                return match val {
                    "sha256" => Ok(ObjectFormat::Sha256),
                    _ => Ok(ObjectFormat::Sha1),
                };
            }
        }
    }
    Ok(ObjectFormat::Sha1)
}

/// Print the publish outcome; an import where any ref was rejected is a failure.
fn report_publish(id: &gitcask_git::RepoId, result: &gitcask_wal::PublishResult) -> Result<()> {
    let rejected: Vec<_> = result
        .per_ref
        .iter()
        .filter_map(|(name, r)| r.as_ref().err().map(|e| (name, e)))
        .collect();
    info!(
        seq = result.seq,
        refs = result.per_ref.len(),
        rejected = rejected.len(),
        "import published"
    );
    println!(
        "imported {} ({} refs, {} rejected, seq {})",
        id,
        result.per_ref.len(),
        rejected.len(),
        result.seq
    );
    if !rejected.is_empty() {
        for (name, err) in rejected.iter().take(20) {
            eprintln!("  rejected {name}: {err:?}");
        }
        if rejected.len() > 20 {
            eprintln!("  ... {} more", rejected.len() - 20);
        }
        bail!("{} ref update(s) rejected during import", rejected.len());
    }
    if result.seq == 0 {
        bail!("import published nothing (seq 0)");
    }
    Ok(())
}

/// Import by copying the source's packfiles verbatim (no re-delta, no index-pack).
/// Loose objects are packed into one extra pack first so nothing is lost. Packs are
/// published as PUSH entries; the last one carries the ref transaction.
async fn import_reusing_packs(
    handle: &Arc<gitcask_wal::RepoHandle>,
    id: &gitcask_git::RepoId,
    git_dir: &std::path::Path,
    updates: Vec<gitcask_proto::v1::RefUpdate>,
) -> Result<()> {
    let started = Instant::now();
    // Loose objects (if any) -> one pack inside the source repo.
    let loose = Command::new("git")
        .args(["count-objects"])
        .current_dir(git_dir)
        .output()
        .context("git count-objects")?;
    let loose_count: u64 = String::from_utf8_lossy(&loose.stdout)
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    if loose_count > 0 {
        info!(loose_count, "packing loose objects in source");
        let st = Command::new("git")
            .args(["repack", "-d"])
            .current_dir(git_dir)
            .status()
            .context("git repack -d")?;
        anyhow::ensure!(st.success(), "git repack -d failed in source");
    }
    let pack_dir = git_dir.join("objects").join("pack");
    let mut packs: Vec<PathBuf> = std::fs::read_dir(&pack_dir)
        .with_context(|| format!("reading {}", pack_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "pack"))
        .collect();
    packs.sort();
    anyhow::ensure!(!packs.is_empty(), "source has no packfiles");
    let local = handle.local();
    let n = packs.len();
    for (i, pack_path) in packs.iter().enumerate() {
        let idx_path = pack_path.with_extension("idx");
        anyhow::ensure!(idx_path.exists(), "missing {}", idx_path.display());
        let stem = pack_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let hex = stem.strip_prefix("pack-").unwrap_or(stem);
        let checksum = gitcask_git::gix_hash::ObjectId::from_hex(hex.as_bytes())
            .with_context(|| format!("pack name is not a checksum: {stem}"))?;
        let pack_size = std::fs::metadata(pack_path)?.len();
        let idx_size = std::fs::metadata(&idx_path)?.len();
        // Object count from the idx fanout (last fanout entry at offset 8 + 255*4).
        let object_count = {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = std::fs::File::open(&idx_path)?;
            f.seek(SeekFrom::Start(8 + 255 * 4))?;
            let mut b = [0u8; 4];
            f.read_exact(&mut b)?;
            u32::from_be_bytes(b) as u64
        };
        // Copy (not move) into a staging dir with the FINAL file names, then let
        // install_pack rename them into objects/pack (it keeps file names).
        let dst_pack = local.pack_path(&checksum);
        let dst_idx = dst_pack.with_extension("idx");
        let staging = local.path().join("objects").join("import-staging");
        std::fs::create_dir_all(&staging)?;
        let file_name = |ext: &str| staging.join(format!("pack-{}.{ext}", checksum.to_hex()));
        let tmp_pack = file_name("pack");
        let tmp_idx = file_name("idx");
        info!(pack = %checksum, pack_size, object_count, "copying pack into local repo");
        tokio::fs::copy(pack_path, &tmp_pack).await?;
        tokio::fs::copy(&idx_path, &tmp_idx).await?;
        let mut extra = Vec::new();
        for ext in ["rev", "bitmap"] {
            let src = pack_path.with_extension(ext);
            if src.exists() {
                let tmp = file_name(ext);
                tokio::fs::copy(&src, &tmp).await?;
                extra.push(tmp);
            }
        }
        local.install_pack(&tmp_pack, &tmp_idx, &extra).await?;
        anyhow::ensure!(dst_pack.exists() && dst_idx.exists(), "pack install failed");
        let ingested = gitcask_git::IngestedPack {
            checksum,
            pack_path: dst_pack,
            idx_path: dst_idx,
            pack_size,
            idx_size,
            object_count,
        };
        let txn = gitcask_proto::v1::RefTransaction {
            updates: if i + 1 == n {
                updates.clone()
            } else {
                Vec::new()
            },
            push_options: Vec::new(),
            atomic: true,
        };
        let result = handle
            .publish_push(
                Some(ingested),
                txn,
                std::collections::HashMap::from([(
                    "imported_from".to_string(),
                    git_dir.display().to_string(),
                )]),
            )
            .await?;
        println!(
            "pack {}/{}: {} bytes, {} objects, seq {}, {:.0} s",
            i + 1,
            n,
            pack_size,
            object_count,
            result.seq,
            started.elapsed().as_secs_f64()
        );
        if i + 1 == n {
            report_publish(id, &result)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod ref_filter_tests {
    use super::*;

    #[test]
    fn default_keeps_heads_and_tags_only() {
        let f = RefFilter::new(vec![]);
        assert!(f.keep("refs/heads/main", "refs/heads/main"));
        assert!(f.keep("refs/heads/feature/x", ""));
        assert!(f.keep("refs/tags/v1.0", ""));
        assert!(!f.keep("refs/remotes/origin/main", ""));
        assert!(!f.keep("refs/pull/1/head", ""));
        assert!(!f.keep("refs/notes/commits", ""));
        // HEAD's target is kept even when unusual.
        assert!(f.keep("refs/weird/head", "refs/weird/head"));
    }

    #[test]
    fn globs_select_exactly() {
        let f = RefFilter::new(vec![
            "refs/heads/main".into(),
            "refs/tags/v*".into(),
            "refs/heads/release-*".into(),
        ]);
        assert!(f.keep("refs/heads/main", ""));
        assert!(!f.keep("refs/heads/dev", ""));
        assert!(f.keep("refs/tags/v1.2.3", ""));
        assert!(!f.keep("refs/tags/nightly", ""));
        assert!(f.keep("refs/heads/release-2026/08", ""));
        assert!(glob_match("a*c*e", "abcde"));
        assert!(!glob_match("a*c*e", "abcdf"));
        assert!(glob_match("*", "anything/at/all"));
        assert!(glob_match("refs/heads/*", "refs/heads/x/y"));
    }
}

#[cfg(test)]
mod import_roundtrip_tests {
    use super::*;
    use gitcask_config::StoreBackend;
    use gitcask_store::DynStore;

    fn git(dir: &std::path::Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .with_context(|| format!("git {}", args.join(" ")))?;
        anyhow::ensure!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn import_twenty_commits_then_clone_through_server() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let source_arg = source
            .to_str()
            .context("temporary source path is not UTF-8")?;
        git(root.path(), &["init", "-q", "-b", "main", source_arg])?;
        for i in 0..20 {
            std::fs::write(source.join("state.txt"), format!("commit {i}\n"))?;
            git(&source, &["add", "state.txt"])?;
            let message = format!("commit {i}");
            git(
                &source,
                &[
                    "-c",
                    "user.name=gitcask test",
                    "-c",
                    "user.email=gitcask@example.com",
                    "commit",
                    "-q",
                    "-m",
                    &message,
                ],
            )?;
        }

        let memory = gitcask_store::memory::MemoryStore::shared();
        let store: DynStore = memory;
        let mut import_cfg = Config::default();
        import_cfg.store.backend = StoreBackend::Memory;
        import_cfg.store.bucket = "import-roundtrip".into();
        import_cfg.cache.dir = root.path().join("import-cache");
        import_cfg.wal.freshness_ttl = std::time::Duration::ZERO;
        let import_cfg = Arc::new(import_cfg);
        let registry = Registry::new(store.clone(), import_cfg.clone());

        run_with_registry(
            source,
            "import/roundtrip".into(),
            false,
            Vec::new(),
            &import_cfg,
            &registry,
        )
        .await?;

        let mut server_cfg = (*import_cfg).clone();
        server_cfg.server.listen = "127.0.0.1:8201".parse()?;
        server_cfg.cache.dir = root.path().join("server-cache");
        server_cfg.validate()?;
        let state = gitcask_server::AppState::new(Arc::new(server_cfg), store).await?;
        let server = tokio::spawn(gitcask_server::serve(state, std::future::pending::<()>()));

        let mut listening = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect("127.0.0.1:8201")
                .await
                .is_ok()
            {
                listening = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        anyhow::ensure!(listening, "import test server did not listen on port 8201");

        let clone = root.path().join("clone");
        let clone_arg = clone
            .to_str()
            .context("temporary clone path is not UTF-8")?;
        git(
            root.path(),
            &[
                "clone",
                "-q",
                "http://127.0.0.1:8201/import/roundtrip.git",
                clone_arg,
            ],
        )?;
        assert_eq!(git(&clone, &["rev-list", "--count", "HEAD"])?, "20");
        assert_eq!(
            std::fs::read_to_string(clone.join("state.txt"))?,
            "commit 19\n"
        );
        git(&clone, &["fsck", "--full"])?;

        server.abort();
        let _ = server.await;
        Ok(())
    }
}
