//! `gitcask` (full CLI: serve | compact | repo | wal | synth | import | migrate)
//! and `gitcask-server` (`gitcask serve` under the name a standalone deployment expects, D39),
//! both thin bins over this library.
//!
//! The only flag is the global `--config PATH` (D8); no subcommand = `serve`. Every command loads
//! `gitcask.toml`, applies `GITCASK__` env overrides, and initialises tracing
//! from `[telemetry]` before dispatching.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cli;
mod synth;

mod compact;
mod import;
mod migrate;
mod repo;
mod serve;
mod token;
mod wal_cmd;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use gitcask_config::Config;
use gitcask_server::telemetry::tracing_init;

#[derive(Parser)]
#[command(
    name = "gitcask",
    version = gitcask_server::health::BUILD_SHA,
    about = "Git at any scale, on object storage, in Rust"
)]
struct Cli {
    /// Path to the configuration file.
    #[arg(
        long,
        global = true,
        env = "GITCASK_CONFIG",
        default_value = "gitcask.toml"
    )]
    config: PathBuf,

    /// No subcommand = `serve`.
    #[command(subcommand)]
    command: Option<Command>,
}

/// `gitcask-server`: the server and nothing else.
#[derive(Parser)]
#[command(name = "gitcask-server", version = gitcask_server::health::BUILD_SHA, about = "gitcask, standalone: git at any scale on an object-storage bucket")]
struct ServerCli {
    /// Path to the configuration file.
    #[arg(
        long,
        global = true,
        env = "GITCASK_CONFIG",
        default_value = "gitcask.toml"
    )]
    config: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server (smart HTTP v0/v2, LFS, and optional compaction loops).
    Serve,
    /// Trigger compaction (geometric repack) for one repository.
    Compact {
        /// `owner/name`.
        repo: String,
        /// Run once and exit (no loop).
        #[arg(long)]
        once: bool,
    },
    /// Repository management.
    Repo {
        #[command(subcommand)]
        action: RepoAction,
    },
    /// WAL inspection and rewind.
    Wal {
        #[command(subcommand)]
        action: WalAction,
    },
    /// Generate a deterministic synthetic repository via `git fast-import`.
    Synth {
        /// Output directory (must not exist or be empty).
        #[arg(long)]
        out: PathBuf,
        /// Repo size preset: s, m, l.
        #[arg(long)]
        size: SynthSize,
        /// Override commit count.
        #[arg(long)]
        commits: Option<u64>,
        /// Override file count.
        #[arg(long)]
        files: Option<u64>,
        /// PRNG seed for deterministic output.
        #[arg(long)]
        seed: Option<u64>,
    },
    /// Import an existing git repository into gitcask.
    Import {
        /// Path to the source `.git` directory or working tree.
        #[arg(long)]
        from: PathBuf,
        /// Target repo id `owner/name`.
        repo: String,
        /// Copy the source's existing packfiles as-is instead of re-packing with
        /// `git pack-objects` (fast for large, already well-packed repos; no
        /// additional compaction is left to the maintainer).
        #[arg(long)]
        reuse_packs: bool,
        /// Ref globs to publish (repeatable; `*` matches anything incl. `/`), e.g.
        /// `--refs refs/heads/main --refs 'refs/tags/v*'`. Default: refs/heads/* and
        /// refs/tags/* (never refs/remotes/*, refs/pull/*, notes); HEAD's target is always kept.
        #[arg(long = "refs")]
        refs: Vec<String>,
    },
    /// Migrate repositories from another git host.
    Migrate {
        #[command(subcommand)]
        source: MigrateSource,
    },
    /// Generate Ed25519 keys or mint a short-lived repository-scoped JWT.
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
}

#[derive(Subcommand)]
enum TokenAction {
    /// Mint an `EdDSA` JWT offline with an issuer-held private key.
    Mint {
        /// PKCS#8 PEM Ed25519 private-key file.
        #[arg(long)]
        key: PathBuf,
        /// Opaque principal string placed in `sub`.
        #[arg(long)]
        principal: String,
        /// Repository scope `<owner>/<repo>:read|write|admin` (repeatable).
        #[arg(long = "scope", required = true)]
        scopes: Vec<String>,
        /// Token lifetime.
        #[arg(long, default_value = "1h")]
        ttl: humantime::Duration,
    },
    /// Generate an Ed25519 private/public key pair without overwriting files.
    Keygen {
        #[arg(long, default_value = "gitcask-private.pem")]
        private_key: PathBuf,
        #[arg(long, default_value = "gitcask-public.pem")]
        public_key: PathBuf,
    },
}

#[derive(Subcommand)]
enum MigrateSource {
    /// Migrate repositories owned by a Gitea user or organization.
    Gitea(migrate::GiteaArgs),
}

#[derive(Subcommand)]
enum RepoAction {
    /// Create a new repository.
    Create {
        /// `owner/name`.
        repo: String,
        /// `sha1` or `sha256`.
        #[arg(long, default_value = "sha1")]
        object_format: String,
    },
    /// Show details for one repository.
    Info {
        /// `owner/name`.
        repo: String,
    },
}

#[derive(Subcommand)]
enum WalAction {
    /// List repositories with pending maintenance markers.
    Pending,
    /// List WAL entries for a repo.
    Ls {
        /// `owner/name`.
        repo: String,
        #[arg(long)]
        from: Option<u64>,
        #[arg(long)]
        to: Option<u64>,
    },
    /// Show one WAL entry.
    Show {
        /// `owner/name`.
        repo: String,
        /// Sequence number.
        seq: u64,
    },
    /// Materialize the repo at a historical sequence into a fresh directory.
    Materialize {
        /// `owner/name`.
        repo: String,
        #[arg(long)]
        at_seq: u64,
        #[arg(long)]
        out: PathBuf,
    },
    /// Publish an already built pack (`pack-<checksum>.pack` + `.idx`) as a
    /// COMPACT entry superseding nothing.
    AddPack {
        /// `owner/name`.
        repo: String,
        /// Path to `pack-<checksum>.pack` (the `.idx` must sit next to it).
        pack: PathBuf,
        #[arg(long, default_value_t = 1)]
        tier: u32,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum SynthSize {
    /// 50 commits, 200 files.
    S,
    /// 2k commits, 5k files, binary blobs, 20 branches, 50 tags.
    M,
    /// 50k commits, 50k files.
    L,
}

fn load_config(path: &std::path::Path) -> Config {
    if path.exists() {
        match Config::load(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "gitcask: error loading config from {}: {e:#}",
                    path.display()
                );
                std::process::exit(1);
            }
        }
    } else {
        // Never run against defaults by accident: a typo'd --config would open the
        // default bucket with whatever credentials are around. Defaults + GITCASK__
        // env on purpose: `--config /dev/null`.
        eprintln!(
            "gitcask: config file {} not found (pass --config PATH or GITCASK_CONFIG; `--config /dev/null` for defaults + GITCASK__ env)",
            path.display()
        );
        std::process::exit(2);
    }
}

pub fn main() -> Result<()> {
    let cli = Cli::parse();
    run(&cli.config, cli.command.unwrap_or(Command::Serve))
}

pub fn main_server() -> Result<()> {
    let cli = ServerCli::parse();
    run(&cli.config, Command::Serve)
}

fn run(config: &std::path::Path, command: Command) -> Result<()> {
    if let Command::Token {
        action: TokenAction::Keygen {
            private_key,
            public_key,
        },
    } = &command
    {
        return token::keygen(private_key, public_key);
    }
    let cfg = load_config(config);
    tracing_init(&cfg);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move { dispatch(command, cfg).await })
}

async fn dispatch(command: Command, cfg: Config) -> Result<()> {
    let cfg = std::sync::Arc::new(cfg);
    match command {
        Command::Synth {
            out,
            size,
            commits,
            files,
            seed,
        } => synth::run(out, size, commits, files, seed).await,
        Command::Serve => serve::run(&cfg).await,
        Command::Compact { repo, once } => compact::run(repo, once, &cfg).await,
        Command::Repo { action } => repo::run(action, &cfg).await,
        Command::Wal { action } => wal_cmd::run(action, &cfg).await,
        Command::Import {
            from,
            repo,
            reuse_packs,
            refs,
        } => import::run(from, repo, reuse_packs, refs, &cfg).await,
        Command::Migrate { source } => match source {
            MigrateSource::Gitea(args) => migrate::run_gitea(args, &cfg).await,
        },
        Command::Token {
            action:
                TokenAction::Mint {
                    key,
                    principal,
                    scopes,
                    ttl,
                },
        } => token::mint(&cfg, &key, &principal, &scopes, ttl.into()).await,
        Command::Token {
            action: TokenAction::Keygen { .. },
        } => Ok(()),
    }
}
