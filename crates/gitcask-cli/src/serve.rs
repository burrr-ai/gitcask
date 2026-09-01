//! `gitcask serve` — run the HTTP server with an optional compaction loop.
//!
//! Opens the object store from config, builds `AppState` (which constructs the
//! WAL registry, authenticator, semaphores, and metrics), then calls
//! `gitcask_server::serve`. When the instance's roles include `compact`, a
//! background loop is spawned:
//!
//!   * **compact loop** — every 60s, for every materialized repo, if
//!     the compaction trigger is met (tier-0 packs ≥ `trigger_packs` or bytes
//!     ≥ `trigger_bytes`) and the compaction lease can be acquired, run
//!     `LocalRepo::repack(geometric)` → `RepoHandle::publish_compact`.
//!   * **maintain** role — `gitcask_server::maintain::run_loop`: checkpoint-if-due
//!     (refs-level) and geometric compaction for every repo, each as a task. It
//!     subsumes the loop above so work is not done twice.

use std::sync::Arc;

use anyhow::Result;
use tokio::signal;
use tracing::{info, warn};

use gitcask_config::{Config, Role};
use gitcask_server::{AppState, serve};
use gitcask_store::open_store;

pub async fn run(cfg: &Arc<Config>) -> Result<()> {
    info!(backend = ?cfg.store.backend, "opening store");
    let store = open_store(cfg).await?;
    info!(backend = store.backend(), "store ready");

    // Ensure the cache directory exists.
    std::fs::create_dir_all(&cfg.cache.dir).ok();

    // AppState::new constructs the registry, auth, semaphores, and metrics.
    let state = AppState::new(cfg.clone(), store).await?;

    // Spawn background loops for non-serving roles.
    let mut bg_handles = Vec::new();

    let maintainer = cfg.has_role(Role::Maintain);
    if maintainer {
        let st = state.clone();
        bg_handles.push(tokio::spawn(async move {
            gitcask_server::maintain::run_loop(st).await;
        }));
    }

    if !maintainer && cfg.has_role(Role::Compact) {
        let reg = state.registry.clone();
        let c = cfg.clone();
        bg_handles.push(tokio::spawn(async move {
            compact_loop(reg, c).await;
        }));
    }

    // Graceful shutdown on SIGTERM / SIGINT.
    let shutdown = async {
        #[cfg(unix)]
        {
            let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
            let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
                .expect("install SIGINT handler");
            tokio::select! {
                _ = sigterm.recv() => info!("received SIGTERM, shutting down"),
                _ = sigint.recv() => info!("received SIGINT, shutting down"),
            }
        }
        #[cfg(not(unix))]
        {
            signal::ctrl_c().await.expect("ctrl_c");
            info!("received Ctrl-C, shutting down");
        }
    };

    info!(listen = %cfg.server.listen, "starting server");
    let result = serve(state.clone(), shutdown).await;

    // Stop background loops before removing the heartbeat so a late pass
    // cannot recreate it after shutdown cleanup.
    for h in bg_handles {
        h.abort();
        let _ = h.await;
    }
    if maintainer {
        gitcask_server::maintain::remove_heartbeat(&state).await;
    }

    result
}

/// Compaction loop: every 60s, check each repo for compaction triggers.
async fn compact_loop(registry: Arc<gitcask_wal::Registry>, cfg: Arc<Config>) {
    if !cfg.compaction.enabled {
        info!("compaction disabled by config, loop exiting");
        return;
    }
    let interval = std::time::Duration::from_secs(60);
    loop {
        tokio::time::sleep(interval).await;
        if let Err(e) = run_compaction_pass(&registry, &cfg).await {
            warn!(error = %e, "compaction pass failed");
        }
    }
}

async fn run_compaction_pass(registry: &gitcask_wal::Registry, cfg: &Config) -> anyhow::Result<()> {
    let repos = registry.cached_repos();
    for id in repos {
        let handle = match registry.open(&id).await {
            Ok(h) => h,
            Err(e) => {
                warn!(repo = %id, error = %e, "failed to open repo for compaction");
                continue;
            }
        };
        let log = |line: String| info!(repo = %id, "{line}");
        match gitcask_server::ops::compact_repo(
            &handle,
            cfg,
            gitcask_server::ops::CompactRequest::default(),
            &log,
        )
        .await
        {
            Ok(outcome) => info!(repo = %id, "{}", outcome.summary()),
            Err(e) => warn!(repo = %id, error = %e, "compaction failed"),
        }
    }
    Ok(())
}
