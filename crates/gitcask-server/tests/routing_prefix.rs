//! D26/D27 + no-compat banner: **repo prefix first, lane segment second**.
//! Source-level (grep), not HTTP.
//!
//! * Repo-scoped routes start with `/{owner}/{repo}` then `/api` or `/api-browser`.
//! * Lane-first repo forms are **gone**: `/api/v1/repos` and
//!   `/services/api/{owner}/{repo}` (nginx rewrite of those too).
//! * Non-repo survivor: `/api/v1` discovery.
//! * Clients must not emit the deleted lane-first repo forms.

use std::fs;
use std::path::{Path, PathBuf};

type TestResult = anyhow::Result<()>;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Non-repo routes only (AGENTS.md D26). Repo aliases are **not** allowed.
fn allowed_route(path: &str) -> bool {
    let p = path.trim();
    let allow = ["/metrics", "/healthz", "/readyz"];
    if allow.contains(&p) {
        return true;
    }
    // Non-repo API (D27): /api/v1 discovery — never /api/v1/repos.
    if p.starts_with("/api/v1") && !p.contains("{repo}") && !p.contains("/repos/") {
        return true;
    }
    // Repo prefix first, then optional lane: /{o}/{r}, /{o}/{r}/api, /{o}/{r}/api-browser, …
    p.starts_with("/{owner}/{repo}")
}

fn route_literals(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        // `.route("/path"` or `.route("/path",`
        let Some(idx) = t.find(".route(\"") else {
            continue;
        };
        let rest = &t[idx + ".route(\"".len()..];
        let Some(end) = rest.find('"') else {
            continue;
        };
        out.push((i + 1, rest[..end].to_string()));
    }
    out
}

#[test]
fn repo_scoped_routes_start_with_owner_repo() -> TestResult {
    let files = [
        "crates/gitcask-server/src/lib.rs",
        "crates/gitcask-server/src/web/api/mod.rs",
        "crates/gitcask-server/src/web/v1.rs",
        "crates/gitcask-server/src/web/status.rs",
    ];
    let mut bad = Vec::new();
    for f in files {
        for (line, path) in route_literals(&read(f)) {
            if !allowed_route(&path) {
                bad.push(format!("{f}:{line}: {path}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "repo-scoped routes must start with /{{owner}}/{{repo}} (or be on the D26 allow-list):\n{}",
        bad.join("\n")
    );
    Ok(())
}

/// Semantics of the edge large-repository location `~ ^/<o>/<r>(?:[./?]|$)`.
#[test]
fn repo_prefix_location_regex_semantics() {
    fn matches(path: &str, repo: &str) -> bool {
        let p = format!("/{repo}");
        path == p
            || path.starts_with(&format!("{p}/"))
            || path.starts_with(&format!("{p}."))
            || path.starts_with(&format!("{p}?"))
    }
    for ok in [
        "/acme/monorepo",
        "/acme/monorepo/",
        "/acme/monorepo.git/info/refs",
        "/acme/monorepo/api/refs",
        "/acme/monorepo/api-browser/refs",
        "/acme/monorepo/info/lfs/objects/aa",
        "/acme/monorepo/tree/main",
    ] {
        assert!(matches(ok, "acme/monorepo"), "{ok}");
    }
    for no in [
        "/acme/monorepowide",
        "/acme/monorepo2",
        "/acme/monorepo-mirror",
        "/acme/monorep",
    ] {
        assert!(!matches(no, "acme/monorepo"), "{no}");
    }
}

/// D27: lane-first **repo** forms are gone. `/{o}/{r}/api-browser` stays.
#[test]
fn deleted_aliases_are_gone() {
    let mut hits = Vec::new();
    for rel in [
        "crates/gitcask-server/src/lib.rs",
        "crates/gitcask-server/src/web/api/mod.rs",
        "crates/gitcask-server/src/web/v1.rs",
        "crates/gitcask-server/src/web/status.rs",
    ] {
        hits.extend(alias_hits(&read(rel), rel));
    }
    assert!(
        hits.is_empty(),
        "lane-first repo aliases still present (/api/v1/repos, /services/api/{{o}}/{{r}}):\n{}",
        hits.join("\n")
    );
}

fn alias_hits(src: &str, rel: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let services_repo_alias =
            t.contains("/services/api/{owner}/{repo}") || t.contains("/services/api/{o}/{r}");
        if t.contains("/api/v1/repos") {
            hits.push(format!("{rel}:{}: /api/v1/repos", i + 1));
        }
        if services_repo_alias {
            hits.push(format!("{rel}:{}: /services/api/{{o}}/{{r}}", i + 1));
        }
    }
    hits
}
