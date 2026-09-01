//! `gitcask repo create|info` — repository management.

use std::sync::Arc;

use anyhow::{Result, bail};
use tracing::info;

use gitcask_config::Config;
use gitcask_git::ObjectFormat;
use gitcask_store::open_store;
use gitcask_wal::Registry;

use crate::RepoAction;
use crate::cli::{parse_repo_id, println_kv};

pub async fn run(action: RepoAction, cfg: &Arc<Config>) -> Result<()> {
    let store = open_store(cfg).await?;
    std::fs::create_dir_all(&cfg.cache.dir).ok();
    let registry = Registry::new(store, cfg.clone());
    match action {
        RepoAction::Create {
            repo,
            object_format,
        } => {
            let (owner, name) = parse_repo_id(&repo)?;
            let id = gitcask_git::RepoId::new(owner, name)?;
            let format = match object_format.as_str() {
                "sha1" => ObjectFormat::Sha1,
                "sha256" => ObjectFormat::Sha256,
                other => bail!("unknown object format `{other}` (expected sha1 or sha256)"),
            };
            let handle = registry.create(&id, format).await?;
            let manifest = handle.manifest();
            println_kv("repo", &id);
            println_kv("object_format", &manifest.object_format);
            println_kv("head_seq", manifest.head_seq);
            info!(repo = %id, "repo created");
        }
        RepoAction::Info { repo } => {
            let (owner, name) = parse_repo_id(&repo)?;
            let id = gitcask_git::RepoId::new(owner, name)?;
            let handle = registry.open(&id).await?;
            handle.sync_full().await?;
            let manifest = handle.manifest();
            let version = handle
                .manifest_version()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "(none)".into());

            println_kv("repo", &id);
            println_kv("object_format", &manifest.object_format);
            println_kv("head_seq", manifest.head_seq);
            println_kv("min_seq", manifest.min_seq);
            println_kv("revision", manifest.revision);
            println_kv("manifest_version", &version);

            let packs = &manifest.packs;
            println_kv("packs", packs.len());
            let total_pack_bytes: u64 = packs.iter().map(|p| p.pack_size).sum();
            println_kv("pack_bytes", total_pack_bytes);

            if let Some(cp) = &manifest.checkpoint {
                println_kv("checkpoint_seq", cp.seq);
                println_kv("checkpoint_key", &cp.key);
            }

            let segments = &manifest.log_segments;
            println_kv("log_segments", segments.len());
            for seg in segments {
                println!(
                    "  {} [{},{}] {} bytes{}",
                    seg.key,
                    seg.first_seq,
                    seg.last_seq,
                    seg.size,
                    if seg.sealed { " (sealed)" } else { "" }
                );
            }
        }
    }
    Ok(())
}
