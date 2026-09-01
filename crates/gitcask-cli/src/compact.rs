//! `gitcask compact REPO [--once]` — trigger compaction manually.
//! Shares the decision/lease/repack/publish logic with the serve loop and the
//! API (`gitcask_server::ops::compact_repo`).

use std::sync::Arc;

use anyhow::{Result, bail};
use tracing::{info, warn};

use gitcask_config::Config;
use gitcask_server::ops::{CompactRequest, compact_repo};
use gitcask_store::open_store;
use gitcask_wal::Registry;

use crate::cli::parse_repo_id;

pub async fn run(repo: String, once: bool, cfg: &Arc<Config>) -> Result<()> {
    if !cfg.compaction.enabled {
        bail!("compaction is disabled in config");
    }

    let store = open_store(cfg).await?;
    std::fs::create_dir_all(&cfg.cache.dir).ok();
    let registry = Registry::new(store, cfg.clone());

    let (owner, name) = parse_repo_id(&repo)?;
    let target_repos = [gitcask_git::RepoId::new(owner, name)?];
    loop {
        for id in &target_repos {
            match compact_one(&registry, id, cfg).await {
                Ok(summary) => println!("{id}: {summary}"),
                Err(e) => warn!(repo = %id, error = %e, "compaction failed"),
            }
        }
        if once {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
    Ok(())
}

async fn compact_one(
    registry: &Registry,
    id: &gitcask_git::RepoId,
    cfg: &Config,
) -> Result<String> {
    let handle = registry.open(id).await?;
    let log = |line: String| {
        info!(repo = %id, "{line}");
        println!("{id}: {line}");
    };
    let outcome = compact_repo(&handle, cfg, CompactRequest { force: false }, &log).await?;
    Ok(outcome.summary())
}
