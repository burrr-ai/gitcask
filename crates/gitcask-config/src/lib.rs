//! `gitcask.toml` — the only configuration surface. Environment overrides use
//! `GITCASK__SECTION__KEY=value` (double underscore = nesting), applied after
//! the file is parsed. `PORT` (a serverless host) overrides `server.listen` port.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
pub use bytesize::ByteSize;
use serde::{Deserialize, Serialize};
pub use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub store: StoreConfig,
    pub cache: CacheConfig,
    pub wal: WalConfig,
    pub compaction: CompactionConfig,
    pub maintenance: MaintenanceConfig,
    pub lfs: LfsConfig,
    pub git: GitConfig,
    /// Links to the systems around a repository.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    pub events: EventsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    pub jwt: JwtConfig,
    pub introspect: IntrospectConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IntrospectConfig {
    pub url: String,
    /// Environment variable containing the service bearer secret, read at startup.
    pub secret_env: String,
    #[serde(with = "humantime_serde")]
    pub cache_ttl: Duration,
    #[serde(with = "humantime_serde")]
    pub negative_cache_ttl: Duration,
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JwtConfig {
    /// Remote JSON Web Key Set. Exactly one of this and `public_key` is required
    /// when `server.auth_mode = "jwt"`.
    pub jwks_url: Option<String>,
    /// Ed25519 public key as PEM text or a path to a PEM file.
    pub public_key: Option<String>,
    pub issuer: String,
    pub audience: Option<String>,
    #[serde(with = "humantime_serde")]
    pub leeway: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub max_concurrent_requests: usize,
    /// Per-repo cap on concurrent upload-pack/receive-pack processes.
    pub max_concurrent_per_repo: usize,
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    /// Graceful drain after SIGTERM: new object work (fetch/push/LFS) is
    /// refused with 503 + Retry-After and `/readyz` turns 503 at once; in-flight
    /// requests and the running maintenance unit get this long to finish
    /// before they are interrupted. Keep it below the process supervisor's stop
    /// grace; longer SSD-host jobs should be resumable rather than relying on drain.
    #[serde(with = "humantime_serde")]
    pub drain_timeout: Duration,
    /// Max size of a single pushed pack accepted over HTTP.
    pub max_push_bytes: ByteSize,
    /// Roles this instance performs. a serverless host: fronts get ["serve"], the
    /// single maintenance instance `["maintain"]` (checkpoint / compact work
    /// driven by pending markers; `compact` is its sub-role). Empty = all.
    pub roles: Vec<Role>,
    pub auth_mode: AuthMode,
    /// Public base URL used when rendering absolute LFS URIs.
    pub public_url: Option<String>,
    /// Create a repo on the first receive-pack push if it does not exist.
    pub auto_create_on_push: bool,
    /// Honour `X-Gitcask-Capabilities: accel-redirect` from an nginx edge
    /// (`deploy/nginx.conf.example`): static LFS objects are answered with
    /// `X-Accel-Redirect: /_store/` + `X-Gitcask-Store-Url` (and `-Authorization`) and no
    /// body, so nginx streams (and caches) the bytes itself. Only turn it on behind an
    /// edge that strips the capability header from clients: the answer carries a store
    /// credential. Off by default. Even when on, accel is honoured only for loopback
    /// peers (the example nginx talks to `127.0.0.1`) so a client on a public bind
    /// cannot spoof the capability header and steal the store credential.
    pub accel_redirect: bool,
    /// Browser origins allowed to call `/api/*` cross-origin through the front proxy.
    /// Exact origins or one
    /// leading `*.` wildcard per entry, e.g. `["https://*.docs.example.com"]`.
    /// Empty (default) = no cross-origin lane and no CORS headers. Non-empty:
    /// CORS with credentials for matching origins only, and state-changing
    /// methods require a matching `Origin` when one is sent.
    pub cors_origins: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Serve,
    Compact,
    /// Background maintenance loop: checkpoint-if-due (refs-level), geometric
    /// compaction for repos whose pack set fits. Implies `Compact`.
    Maintain,
    /// The events bridge (`docs/EVENTS.md`): tails every repo's WAL from a
    /// per-repo cursor and publishes `ref` events to the webhook. Woken by
    /// `POST /_events/notify` and a periodic sweep. A small separate service
    /// in production.
    Events,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Everyone is `anon` with write **and admin**. `Config::validate` refuses this
    /// unless `server.listen` is loopback.
    #[default]
    None,
    /// Verify `EdDSA` JWTs locally and apply repository scopes.
    Jwt,
    /// Verify opaque tokens with the issuer and apply the same repository scopes.
    Introspect,
    /// Select introspection or secret-verified forwarding per request.
    IntrospectForwarded,
    /// Trust identity and permission headers injected by the front proxy.
    Forwarded,
}

/// Read the required shared proxy secret for combined authentication. Discard
/// environment errors because `VarError::NotUnicode` contains the secret value.
pub fn required_forward_secret() -> Result<String> {
    let secret = std::env::var("GITCASK_FORWARD_SECRET")
        .map_err(|_| anyhow::anyhow!("introspect_forwarded requires GITCASK_FORWARD_SECRET"))?;
    anyhow::ensure!(
        !secret.is_empty() && secret.bytes().all(|byte| byte.is_ascii_graphic()),
        "introspect_forwarded requires a non-empty printable ASCII GITCASK_FORWARD_SECRET without whitespace"
    );
    Ok(secret)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StoreConfig {
    pub backend: StoreBackend,
    pub bucket: String,
    /// Global key prefix inside the bucket (no leading slash; trailing slash added).
    pub prefix: String,
    pub s3: S3Config,
    /// Retry cap for transient store failures. Each delay is capped at 2 s,
    /// so the default four retries add at most 8 s to a request.
    pub max_retries: u32,
    /// Objects larger than this use resumable/multipart upload.
    pub multipart_threshold: ByteSize,
    pub multipart_part_size: ByteSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StoreBackend {
    #[default]
    S3,
    /// Tests only.
    Memory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct S3Config {
    pub endpoint: String,
    pub region: String,
    pub credentials: S3CredentialsMode,
    pub access_key_env: String,
    pub secret_key_env: String,
    pub force_path_style: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum S3CredentialsMode {
    #[default]
    Static,
    Default,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CacheConfig {
    /// Local directory holding materialized repositories.
    pub dir: PathBuf,
    /// Async worker threads dedicated to pack materialization. Blocking
    /// filesystem and git work runs on this runtime's blocking pool instead.
    pub bulk_threads: usize,
    /// Evict idle repos (oldest first) when the filesystem holding
    /// `dir` is fuller than this fraction (0 = never evict on pressure).
    pub disk_high_watermark: f64,
    #[serde(with = "humantime_serde")]
    pub evict_idle_after: Duration,
    /// How often serving instances run local cache eviction.
    #[serde(with = "humantime_serde")]
    pub evict_interval: Duration,
    /// Max entries in the ref advertisement cache (per rendered ls-refs / v0 advert).
    pub ref_advert_entries: usize,
    /// Mirror rendered sha-addressed web API responses into the object store
    /// (`repos/<o>/<r>/cache/api/...`) so every instance shares one render cache.
    pub shared_render_cache: bool,
    /// Delete shared render and archive cache objects older than this during
    /// the next repository bucket-GC unit.
    #[serde(with = "humantime_serde")]
    pub shared_retention: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WalConfig {
    /// Coalesce concurrent publishes to one repo within this window into one index CAS.
    #[serde(with = "humantime_serde")]
    pub batch_window: Duration,
    pub max_batch: usize,
    /// Checkpoint (ref snapshot + pack set, log folded) when this many entries
    /// accumulated since the last one (0 = never by count).
    pub snapshot_every_entries: u64,
    /// ... or when the last checkpoint is older than this (0 = never by age).
    /// Cold readers load checkpoint + tail, so the tail stays short.
    #[serde(with = "humantime_serde")]
    pub checkpoint_interval: Duration,
    /// ... or when the log tail after the checkpoint exceeds this many bytes
    /// (0 = never by size).
    pub checkpoint_tail_bytes: ByteSize,
    pub cas_max_retries: u32,
    /// Verify pushed objects (fsck-level) before publish.
    pub fsck_objects: bool,
    /// Require every pushed ref tip to be connected to existing objects + the pack.
    pub check_connectivity: bool,
    /// Skip the index freshness GET if the last check was younger than this (0 = always check).
    #[serde(with = "humantime_serde")]
    pub freshness_ttl: Duration,
    /// After a refs-only sync (info/refs, ls-refs, web refs) on a
    /// copy whose packs are not yet reconciled, start downloading the packs in
    /// the background so the first fetch does not pay for it.
    pub prefetch_packs: bool,
}

/// The `maintain` role's loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MaintenanceConfig {
    /// How long to wait before listing again when there are no pending markers.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Repositories maintained concurrently. Each worker may materialize one
    /// complete repository, so size this with the cache's disk and memory headroom.
    pub workers: usize,
    /// Maximum pending repository markers consumed by one pass.
    pub max_repos_per_pass: usize,
    /// Checkpoint repos whose trigger fired (see `wal.snapshot_every_entries`,
    /// `wal.checkpoint_interval`, `wal.checkpoint_tail_bytes`). Time-based
    /// triggers are evaluated only for repositories with a pending marker;
    /// repositories without pushes are left for explicit CLI maintenance.
    pub checkpoints: bool,
    /// Heartbeat object name (`maintain/<host>.pb`): capacity and whether that
    /// host is alive. Default: the instance id.
    #[serde(default)]
    pub host: Option<String>,
    /// A maintainer heartbeat older than this is expired and removed.
    #[serde(with = "humantime_serde")]
    pub heartbeat_ttl: Duration,
    /// Connectivity audit cadence after a prior audit and a newer push. A
    /// compaction is audited immediately; a never-compacted new repository is
    /// not audited on first visit. `git fsck --connectivity-only` runs over a
    /// complete local copy and records `repos/<o>/<r>/fsck.pb` (missing objects
    /// → `gitcask_repo_missing_objects{repo}`). Lowest priority; 0 removes the
    /// age delay but still requires a prior audit and newer push.
    #[serde(with = "humantime_serde")]
    pub fsck_interval: Duration,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        MaintenanceConfig {
            interval: Duration::from_secs(60),
            workers: 8,
            max_repos_per_pass: 1000,
            checkpoints: true,
            host: None,
            heartbeat_ttl: Duration::from_hours(1),
            fsck_interval: Duration::from_secs(7 * 24 * 3600),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CompactionConfig {
    pub enabled: bool,
    /// Geometric factor between tiers.
    pub factor: u32,
    /// Compact when this many fresh (tier 0) packs exist.
    pub trigger_packs: usize,
    /// Or when fresh pack bytes exceed this.
    pub trigger_bytes: ByteSize,
    /// Per-repository compaction and checkpoint lease TTL.
    #[serde(with = "humantime_serde")]
    pub lease_ttl: Duration,
    /// Keep superseded packs and old index generations for this long (provenance/rewind).
    #[serde(with = "humantime_serde")]
    pub retention_superseded: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LfsServe {
    #[default]
    Proxy,
    SignedUrl,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LfsConfig {
    pub enabled: bool,
    pub serve_via: LfsServe,
    #[serde(with = "humantime_serde")]
    pub signed_url_ttl: Duration,
    pub max_object_bytes: ByteSize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GitConfig {
    pub allow_filter: bool,
    pub allow_any_sha1_in_want: bool,
    /// Default object format for new repos.
    pub object_format: ObjectFormat,
    /// Maintain a split commit-graph chain per local repository.
    #[serde(default = "default_true")]
    pub commit_graph: bool,
    /// Compute changed-path Bloom filters for incremental layers (`git log --
    /// path` speed-up). Diffs new commits against parent trees, so it needs
    /// the parents' tree data locally reachable.
    #[serde(default)]
    pub commit_graph_changed_paths: bool,
    /// Refuse a v2 `fetch` asking for more than this many objects (0 = no bound). A blobless clone
    /// without `--sparse`/`--no-checkout` makes git fetch every blob of HEAD's tree in one lazy
    /// request right after cloning (a large repository: 1.47 M wants, 49 GB RSS, > 12 min on the SSD host); the
    /// refusal names the fix. Ordinary fetches want a handful of tips; a `--sparse` checkout fetches
    /// its cone's blobs, a few thousand at most. Set it per host above the largest honest request.
    #[serde(default)]
    pub max_wants: usize,
    /// Maximum number of file changes accepted by one commit API request.
    pub max_commit_changes: usize,
    /// Maximum total decoded blob bytes accepted by one commit API request.
    pub max_commit_bytes: ByteSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObjectFormat {
    #[default]
    Sha1,
    Sha256,
}

/// Events (`docs/EVENTS.md`): the bridge (`Role::Events`) tails every repo's
/// WAL from a durable cursor and publishes `ref` events to the webhook. Only the
/// bridge reads this section; nothing on the push path does.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EventsConfig {
    /// Where ref events go: each batch is `POST`ed as a JSON array (`docs/EVENTS.md`).
    /// Unset = the events role has nothing to do.
    pub webhook_url: Option<String>,
    /// Shared secret for `X-Gitcask-Signature: sha256=<HMAC-SHA256 of the body>`. Unset = unsigned.
    pub webhook_secret: Option<String>,
    /// Backstop sweep over pending markers plus repositories cached on this
    /// instance. The bridge warns when it finds unpublished entries. `0` = off.
    #[serde(with = "humantime_serde")]
    pub sweep_interval: Duration,
}

impl Default for EventsConfig {
    fn default() -> Self {
        EventsConfig {
            webhook_url: None,
            webhook_secret: None,
            sweep_interval: Duration::from_secs(300),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TelemetryConfig {
    /// `json` (Cloud Logging) or `pretty`.
    pub log_format: LogFormat,
    pub log_filter: String,
    /// Prometheus scrape endpoint on the main listener (`/metrics`).
    pub metrics: bool,
    /// GCP project id for Cloud Logging trace correlation.
    /// Falls back to env `GOOGLE_CLOUD_PROJECT` then the metadata server.
    pub trace_project: Option<String>,
    /// A wait on a per-repository lock or a store permit on a request path (`RepoHandle::rw`,
    /// `sync_mutex`, `pack_mutex`, or a store permit) longer than this is logged as a WARN
    /// `lock wait` line with `lock`, `repo`, `wait_ms` (+ the request's id from the span); every
    /// wait that was not immediately satisfied lands in `gitcask_lock_wait_seconds{lock}`. D19's
    /// incident (a queued writer starving readers for 60–680 s) is what this makes visible.
    #[serde(with = "humantime_serde")]
    pub lock_wait_warn: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Json,
    Pretty,
}

fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server: ServerConfig::default(),
            auth: AuthConfig::default(),
            store: StoreConfig::default(),
            cache: CacheConfig::default(),
            wal: WalConfig::default(),
            compaction: CompactionConfig::default(),
            maintenance: MaintenanceConfig::default(),
            lfs: LfsConfig::default(),
            git: GitConfig::default(),
            telemetry: TelemetryConfig::default(),
            events: EventsConfig::default(),
        }
    }
}

impl Default for JwtConfig {
    fn default() -> Self {
        Self {
            jwks_url: None,
            public_key: None,
            issuer: String::new(),
            audience: None,
            leeway: Duration::from_mins(1),
        }
    }
}

impl Default for IntrospectConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            secret_env: String::new(),
            cache_ttl: Duration::from_secs(30),
            negative_cache_ttl: Duration::from_secs(3),
            timeout: Duration::from_secs(2),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            listen: "127.0.0.1:8080".parse().unwrap(),
            max_concurrent_requests: 512,
            max_concurrent_per_repo: 64,
            request_timeout: Duration::from_secs(3600),
            drain_timeout: Duration::from_secs(20),
            max_push_bytes: ByteSize::gib(64),
            roles: vec![],
            auth_mode: AuthMode::None,
            public_url: None,
            auto_create_on_push: false,
            accel_redirect: false,
            cors_origins: vec![],
        }
    }
}
impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            backend: StoreBackend::S3,
            bucket: "gitcask".into(),
            prefix: String::new(),
            s3: S3Config::default(),
            max_retries: 4,
            multipart_threshold: ByteSize::mib(64),
            multipart_part_size: ByteSize::mib(32),
        }
    }
}
impl Default for S3Config {
    fn default() -> Self {
        S3Config {
            endpoint: "http://127.0.0.1:9000".into(),
            region: "us-east-1".into(),
            credentials: S3CredentialsMode::Static,
            access_key_env: "AWS_ACCESS_KEY_ID".into(),
            secret_key_env: "AWS_SECRET_ACCESS_KEY".into(),
            force_path_style: true,
        }
    }
}
impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            dir: PathBuf::from("/tmp/gitcask"),
            bulk_threads: 2,
            disk_high_watermark: 0.9,
            evict_idle_after: Duration::from_secs(6 * 3600),
            evict_interval: Duration::from_mins(1),
            ref_advert_entries: 256,
            shared_render_cache: true,
            shared_retention: Duration::from_hours(30 * 24),
        }
    }
}
impl Default for WalConfig {
    fn default() -> Self {
        WalConfig {
            batch_window: Duration::from_millis(5),
            max_batch: 64,
            snapshot_every_entries: 256,
            checkpoint_interval: Duration::from_secs(3600),
            checkpoint_tail_bytes: ByteSize::mib(8),
            cas_max_retries: 16,
            fsck_objects: true,
            check_connectivity: true,
            freshness_ttl: Duration::ZERO,
            prefetch_packs: true,
        }
    }
}
impl Default for CompactionConfig {
    fn default() -> Self {
        CompactionConfig {
            enabled: true,
            factor: 2,
            trigger_packs: 16,
            trigger_bytes: ByteSize::gib(1),
            lease_ttl: Duration::from_secs(600),
            retention_superseded: Duration::from_secs(7 * 24 * 3600),
        }
    }
}
impl Default for LfsConfig {
    fn default() -> Self {
        LfsConfig {
            enabled: true,
            serve_via: LfsServe::Proxy,
            signed_url_ttl: Duration::from_secs(3600),
            max_object_bytes: ByteSize::gib(16),
        }
    }
}
impl Default for GitConfig {
    fn default() -> Self {
        GitConfig {
            allow_filter: true,
            allow_any_sha1_in_want: false,
            object_format: ObjectFormat::Sha1,
            commit_graph: true,
            commit_graph_changed_paths: false,
            max_wants: 0,
            max_commit_changes: 1000,
            max_commit_bytes: ByteSize::mib(16),
        }
    }
}
impl Default for TelemetryConfig {
    fn default() -> Self {
        TelemetryConfig {
            log_format: LogFormat::Json,
            log_filter: "info,gitcask=debug".into(),
            metrics: true,
            trace_project: None,
            lock_wait_warn: Duration::from_secs(1),
        }
    }
}

fn secure_auth_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && (url.scheme() == "https"
            || (url.scheme() == "http"
                && match url.host() {
                    Some(url::Host::Domain("localhost")) => true,
                    Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                    Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                    _ => false,
                }))
}

impl Config {
    pub fn parse(toml_text: &str) -> Result<Config> {
        let mut cfg: Config = toml::from_str(toml_text).context("parsing gitcask.toml")?;
        cfg.apply_env(std::env::vars())?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &std::path::Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text)
    }

    /// Apply `GITCASK__a__b=v` overrides (values parsed as TOML values, falling back to string)
    /// and a serverless host's `PORT`.
    ///
    /// Config and image are released independently (the ssd-host host follows
    /// the serving image's version): an override for a key **unknown to this build**
    /// (or with an unparsable value) is **ignored with a WARN**, never a
    /// startup failure (2026-08-21: `unknown field disk_high_watermark`
    /// crash-looped a host running the previous image). The ignored keys are
    /// returned by [`apply_env_report`].
    pub fn apply_env(&mut self, vars: impl Iterator<Item = (String, String)>) -> Result<()> {
        let ignored = self.apply_env_report(vars)?;
        for (k, why) in &ignored {
            tracing::warn!(key = %k, reason = %why, "ignoring {k}: unknown in this build");
        }
        Ok(())
    }

    /// [`apply_env`] returning the `(key, reason)` pairs it had to ignore.
    pub fn apply_env_report(
        &mut self,
        vars: impl Iterator<Item = (String, String)>,
    ) -> Result<Vec<(String, String)>> {
        let mut doc: toml::Table = toml::Table::try_from(&*self).context("serializing config")?;
        let mut touched = false;
        let mut ignored = Vec::new();
        let mut port_override = None;
        for (k, v) in vars {
            if k == "PORT" {
                port_override = v.parse::<u16>().ok();
                continue;
            }
            let Some(rest) = k.strip_prefix("GITCASK__") else {
                continue;
            };
            let path: Vec<String> = rest.split("__").map(|s| s.to_ascii_lowercase()).collect();
            if path.is_empty() || path.iter().any(|p| p.is_empty()) {
                continue;
            }
            let value: toml::Value = v
                .parse::<toml::Value>()
                .unwrap_or(toml::Value::String(v.clone()));
            // Apply into a copy and type-check it alone: a bad/unknown key is
            // dropped (WARN) instead of failing every other override with it.
            let mut trial = doc.clone();
            let bad = {
                fn set(
                    cur: &mut toml::Table,
                    path: &[String],
                    value: toml::Value,
                ) -> std::result::Result<(), String> {
                    if path.len() == 1 {
                        cur.insert(path[0].clone(), value);
                        return Ok(());
                    }
                    let next = cur
                        .entry(path[0].clone())
                        .or_insert_with(|| toml::Value::Table(Default::default()))
                        .as_table_mut()
                        .ok_or_else(|| format!("{} is not a table", path[0]))?;
                    set(next, &path[1..], value)
                }
                match set(&mut trial, &path, value) {
                    Err(why) => Some(why),
                    Ok(()) => trial.clone().try_into::<Config>().err().map(|e| {
                        e.to_string()
                            .lines()
                            .next()
                            .unwrap_or("invalid")
                            .to_string()
                    }),
                }
            };
            match bad {
                Some(why) => ignored.push((k, why)),
                None => {
                    doc = trial;
                    touched = true;
                }
            }
        }
        if touched {
            *self = doc.try_into().context("applying GITCASK__ env overrides")?;
        }
        if let Some(port) = port_override {
            self.server.listen.set_port(port);
            // Keep a loopback public URL's port in lockstep with PORT. A real
            // public URL is left alone.
            if let Some(u) = self.server.public_url.as_mut() {
                if origin_is_loopback(u) {
                    *u = rewrite_origin_port(u, port);
                }
            }
        }
        Ok(ignored)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.store.bucket.is_empty(), "store.bucket must be set");
        if self.server.auth_mode == AuthMode::Jwt {
            let jwt = &self.auth.jwt;
            anyhow::ensure!(
                !jwt.issuer.trim().is_empty(),
                "auth.jwt.issuer must be set in jwt mode"
            );
            anyhow::ensure!(
                jwt.jwks_url.is_some() ^ jwt.public_key.is_some(),
                "jwt mode requires exactly one of auth.jwt.jwks_url or auth.jwt.public_key"
            );
            if let Some(url) = &jwt.jwks_url {
                anyhow::ensure!(
                    secure_auth_url(url),
                    "auth.jwt.jwks_url must use https (http is allowed only on loopback)"
                );
            }
            if let Some(audience) = &jwt.audience {
                anyhow::ensure!(
                    !audience.trim().is_empty(),
                    "auth.jwt.audience may not be empty"
                );
            }
        }
        if matches!(
            self.server.auth_mode,
            AuthMode::Introspect | AuthMode::IntrospectForwarded
        ) {
            let introspect = &self.auth.introspect;
            anyhow::ensure!(
                secure_auth_url(&introspect.url),
                "auth.introspect.url must use https (http is allowed only on loopback)"
            );
            anyhow::ensure!(
                std::env::var(&introspect.secret_env).is_ok_and(|secret| !secret.is_empty()),
                "auth.introspect.secret_env must name a set, non-empty environment variable"
            );
            anyhow::ensure!(
                introspect.cache_ttl <= Duration::from_mins(10),
                "auth.introspect.cache_ttl must be at most 10m"
            );
            anyhow::ensure!(
                !introspect.timeout.is_zero() && introspect.timeout <= Duration::from_secs(10),
                "auth.introspect.timeout must be greater than zero and at most 10s"
            );
        }
        if self.server.auth_mode == AuthMode::IntrospectForwarded {
            required_forward_secret()?;
            anyhow::ensure!(
                std::env::var(&self.auth.introspect.secret_env)
                    .is_ok_and(|secret| secret.bytes().all(|byte| byte.is_ascii_graphic())),
                "introspect_forwarded requires a printable ASCII introspection service secret without whitespace"
            );
        }
        if let Some(u) = &self.server.public_url {
            anyhow::ensure!(
                u.starts_with("https://") || u.starts_with("http://"),
                "server.public_url must be an http(s) origin (got {u})"
            );
        }
        for o in &self.server.cors_origins {
            let host = o
                .strip_prefix("https://")
                .or_else(|| o.strip_prefix("http://localhost").map(|_| "localhost"))
                .filter(|h| !h.is_empty() && !h.contains('/'));
            anyhow::ensure!(
                host.is_some()
                    && o.matches('*').count() <= 1
                    && (!o.contains('*') || o.contains("://*.")),
                "server.cors_origins entries must be https origins (or http://localhost[:port]) with at most one leading `*.` wildcard, got {o:?}"
            );
        }
        // `none` grants write and admin to every request, so it is safe only on loopback.
        if self.server.auth_mode == AuthMode::None {
            anyhow::ensure!(
                self.server.listen.ip().is_loopback(),
                "server.auth_mode = none is loopback-only (listen is {}); use jwt, introspect, forwarded or introspect_forwarded for a public bind",
                self.server.listen
            );
        }
        anyhow::ensure!(
            self.compaction.factor >= 2,
            "compaction.factor must be >= 2"
        );
        anyhow::ensure!(self.wal.max_batch >= 1, "wal.max_batch must be >= 1");
        anyhow::ensure!(
            self.git.max_commit_changes >= 1,
            "git.max_commit_changes must be >= 1"
        );
        anyhow::ensure!(
            self.git.max_commit_bytes.as_u64() >= 1,
            "git.max_commit_bytes must be >= 1 byte"
        );
        anyhow::ensure!(
            self.cache.bulk_threads >= 1,
            "cache.bulk_threads must be >= 1"
        );
        anyhow::ensure!(
            self.maintenance.workers >= 1,
            "maintenance.workers must be >= 1"
        );
        anyhow::ensure!(
            self.maintenance.max_repos_per_pass >= 1,
            "maintenance.max_repos_per_pass must be >= 1"
        );
        if let Some(u) = &self.events.webhook_url {
            anyhow::ensure!(
                u.starts_with("http://") || u.starts_with("https://"),
                "events.webhook_url must be an http(s) URL"
            );
        }
        Ok(())
    }

    /// Store prefix normalized to either "" or "something/".
    pub fn store_prefix(&self) -> String {
        let p = self.store.prefix.trim_matches('/');
        if p.is_empty() {
            String::new()
        } else {
            format!("{p}/")
        }
    }

    pub fn has_role(&self, role: Role) -> bool {
        self.server.roles.is_empty()
            || self.server.roles.contains(&role)
            || (role == Role::Compact && self.server.roles.contains(&Role::Maintain))
    }
}

fn origin_host(origin: &str) -> &str {
    let rest = origin
        .trim_end_matches('/')
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(origin);
    if let Some(inside) = rest.strip_prefix('[') {
        return inside.split_once(']').map(|(h, _)| h).unwrap_or(inside);
    }
    rest.split([':', '/']).next().unwrap_or(rest)
}

fn origin_is_loopback(origin: &str) -> bool {
    let host = origin_host(origin);
    host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1" || host == "::1"
}

fn rewrite_origin_port(origin: &str, port: u16) -> String {
    let origin = origin.trim_end_matches('/');
    let Some((scheme, rest)) = origin.split_once("://") else {
        return origin.to_string();
    };
    let host = if rest.starts_with('[') {
        rest.split_once(']')
            .map(|(h, _)| format!("{h}]"))
            .unwrap_or_else(|| rest.to_string())
    } else {
        rest.split([':', '/']).next().unwrap_or(rest).to_string()
    };
    let default = if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    };
    if port == default {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_and_validate() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.server.listen.port(), 8080);
        assert!(!c.server.auto_create_on_push);
        assert_eq!(c.store.backend, StoreBackend::S3);
        assert_eq!(c.store.s3.credentials, S3CredentialsMode::Static);
        assert_eq!(c.cache.bulk_threads, 2);
        assert_eq!(c.cache.shared_retention, Duration::from_hours(30 * 24));
        c.validate().unwrap();
        // Round trip through TOML.
        let text = toml::to_string(&c).unwrap();
        let back = Config::parse(&text).unwrap();
        assert_eq!(back.store.bucket, c.store.bucket);
    }

    #[test]
    fn s3_credentials_mode_parses() {
        let default = Config::parse("[store.s3]\ncredentials = \"default\"\n").unwrap();
        assert_eq!(default.store.s3.credentials, S3CredentialsMode::Default);

        let static_mode = Config::parse("[store.s3]\ncredentials = \"static\"\n").unwrap();
        assert_eq!(static_mode.store.s3.credentials, S3CredentialsMode::Static);
        assert!(Config::parse("[store.s3]\ncredentials = \"unknown\"\n").is_err());
    }

    #[test]
    fn env_overrides() {
        let mut c = Config::default();
        c.apply_env(
            vec![
                ("GITCASK__STORE__BACKEND".to_string(), "s3".to_string()),
                (
                    "GITCASK__STORE__S3__ENDPOINT".to_string(),
                    "http://rustfs:9000".to_string(),
                ),
                ("GITCASK__WAL__MAX_BATCH".to_string(), "7".to_string()),
                ("PORT".to_string(), "9090".to_string()),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(c.store.backend, StoreBackend::S3);
        assert_eq!(c.store.s3.endpoint, "http://rustfs:9000");
        assert_eq!(c.wal.max_batch, 7);
        assert_eq!(c.server.listen.port(), 9090);
    }

    #[test]
    fn port_rewrites_loopback_public_url_only() {
        let mut c = Config::default();
        c.server.public_url = Some("https://gitcask.localhost:8888".into());
        c.apply_env(vec![("PORT".to_string(), "8080".to_string())].into_iter())
            .unwrap();
        assert_eq!(c.server.listen.port(), 8080);
        assert_eq!(
            c.server.public_url.as_deref(),
            Some("https://gitcask.localhost:8080")
        );

        let mut prod = Config::default();
        prod.server.public_url = Some("https://git.example.com".into());
        prod.apply_env(vec![("PORT".to_string(), "8080".to_string())].into_iter())
            .unwrap();
        assert_eq!(
            prod.server.public_url.as_deref(),
            Some("https://git.example.com")
        );
    }

    /// Config and image release independently: an override for a key this
    /// build does not know (or a value it cannot parse) is ignored and
    /// reported, the known ones still apply, startup continues.
    #[test]
    fn env_override_unknown_key_is_ignored_not_fatal() {
        let mut c = Config::default();
        let ignored = c
            .apply_env_report(
                vec![
                    (
                        "GITCASK__CACHE__NOT_A_KEY_YET".to_string(),
                        "0.9".to_string(),
                    ),
                    (
                        "GITCASK__WAL__MAX_BATCH".to_string(),
                        "not-a-number".to_string(),
                    ),
                    ("GITCASK__NOSUCHSECTION__X".to_string(), "1".to_string()),
                    ("GITCASK__WAL__BATCH_WINDOW".to_string(), "30ms".to_string()),
                ]
                .into_iter(),
            )
            .unwrap();
        assert_eq!(
            c.wal.batch_window,
            Duration::from_millis(30),
            "known override still applied"
        );
        let keys: Vec<&str> = ignored.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "GITCASK__CACHE__NOT_A_KEY_YET",
                "GITCASK__WAL__MAX_BATCH",
                "GITCASK__NOSUCHSECTION__X"
            ]
        );
        assert!(ignored[0].1.contains("unknown field"), "{:?}", ignored[0]);
        // Plain apply_env is the same, just warns.
        let mut c2 = Config::default();
        c2.apply_env(
            vec![("GITCASK__CACHE__NOT_A_KEY_YET".to_string(), "1".to_string())].into_iter(),
        )
        .unwrap();
    }

    #[test]
    fn auth_modes_validate_fail_closed() {
        let forwarded = Config::parse(
            "[store]\nbucket = \"b\"\n[server]\nlisten = \"0.0.0.0:8080\"\nauth_mode = \"forwarded\"\n",
        )
        .unwrap();
        assert_eq!(forwarded.server.auth_mode, AuthMode::Forwarded);
        let err = Config::parse(
            "[store]\nbucket = \"b\"\n[server]\nlisten = \"0.0.0.0:8080\"\nauth_mode = \"none\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("loopback-only"), "{err}");

        let jwt = Config::parse(
            "[server]\nlisten = \"0.0.0.0:8080\"\nauth_mode = \"jwt\"\n[auth.jwt]\nissuer = \"https://issuer.example\"\npublic_key = \"public.pem\"\n",
        )
        .unwrap();
        assert_eq!(jwt.server.auth_mode, AuthMode::Jwt);
        let missing_key =
            Config::parse("[server]\nauth_mode = \"jwt\"\n[auth.jwt]\nissuer = \"issuer\"\n")
                .unwrap_err();
        assert!(
            missing_key.to_string().contains("exactly one"),
            "{missing_key}"
        );
        let two_keys = Config::parse(
            "[server]\nauth_mode = \"jwt\"\n[auth.jwt]\nissuer = \"issuer\"\npublic_key = \"public.pem\"\njwks_url = \"https://issuer.example/jwks\"\n",
        )
        .unwrap_err();
        assert!(two_keys.to_string().contains("exactly one"), "{two_keys}");
    }

    #[test]
    fn events_section_parses_and_validates() {
        let c = Config::parse(
            r#"
[events]
sweep_interval = "1m"
webhook_url = "https://hooks.example.com/gitcask"
webhook_secret = "s"
"#,
        )
        .unwrap();
        assert_eq!(c.events.sweep_interval, Duration::from_secs(60));
        assert_eq!(c.events.webhook_secret.as_deref(), Some("s"));
        let err = Config::parse("[events]\nwebhook_url = \"ftp://x\"\n").unwrap_err();
        assert!(err.to_string().contains("webhook_url"), "{err}");
    }

    #[test]
    fn jwt_section_parses_and_validates() {
        let config = Config::parse(
            r#"
[server]
auth_mode = "jwt"
[auth.jwt]
issuer = "https://issuer.example"
audience = "gitcask"
jwks_url = "https://issuer.example/.well-known/jwks.json"
leeway = "30s"
"#,
        )
        .unwrap();
        assert_eq!(config.auth.jwt.audience.as_deref(), Some("gitcask"));
        assert_eq!(config.auth.jwt.leeway, Duration::from_secs(30));
    }

    #[test]
    fn introspect_config_defaults_and_limits() {
        // Cargo supplies this harmless value to tests; avoid mutating the
        // process environment while other tests run.
        let mut config = Config::parse(
            r#"
[server]
auth_mode = "introspect"
[auth.introspect]
url = "https://issuer.example/introspect"
secret_env = "CARGO_PKG_NAME"
"#,
        )
        .unwrap();
        assert_eq!(config.server.auth_mode, AuthMode::Introspect);
        assert_eq!(config.auth.introspect.cache_ttl, Duration::from_secs(30));
        assert_eq!(
            config.auth.introspect.negative_cache_ttl,
            Duration::from_secs(3)
        );
        assert_eq!(config.auth.introspect.timeout, Duration::from_secs(2));
        config.auth.introspect.cache_ttl = Duration::from_mins(10);
        config.auth.introspect.timeout = Duration::from_secs(10);
        config.validate().unwrap();
        config.auth.introspect.cache_ttl += Duration::from_nanos(1);
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("cache_ttl")
        );
        config.auth.introspect.cache_ttl = Duration::ZERO;
        config.auth.introspect.timeout += Duration::from_nanos(1);
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("timeout")
        );
        config.auth.introspect.timeout = Duration::ZERO;
        assert!(config.validate().is_err());
        config.auth.introspect.timeout = Duration::from_secs(2);
        for name in ["", "GITCASK_TEST_INTROSPECT_UNSET_38", "INVALID=NAME"] {
            config.auth.introspect.secret_env = name.into();
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("secret_env")
            );
        }
    }

    #[test]
    fn introspect_forwarded_startup_validation() -> Result<()> {
        const CHILD: &str = "GITCASK_TEST_COMBINED_CONFIG_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Each child owns its environment, even under parallel cargo test.
            for secret in [
                None,
                Some(""),
                Some(" "),
                Some("bad\nsecret"),
                Some("é"),
                Some("proxy-secret"),
            ] {
                let mut command = std::process::Command::new(std::env::current_exe()?);
                command
                    .args(["--exact", "tests::introspect_forwarded_startup_validation"])
                    .env(CHILD, "1")
                    .env("GITCASK_TEST_COMBINED_EMPTY", "")
                    .env("GITCASK_TEST_COMBINED_INVALID", "bad\nservice-secret")
                    .env_remove("GITCASK_FORWARD_SECRET");
                if let Some(secret) = secret {
                    command.env("GITCASK_FORWARD_SECRET", secret);
                }
                assert!(command.status()?.success());
            }
            return Ok(());
        }
        let mut config = Config::default();
        config.server.auth_mode = AuthMode::IntrospectForwarded;
        config.auth.introspect.url = "https://issuer.example/introspect".into();
        config.auth.introspect.secret_env = "CARGO_PKG_NAME".into();
        if std::env::var("GITCASK_FORWARD_SECRET").as_deref() != Ok("proxy-secret") {
            let error = config.validate().expect_err("invalid proxy secret");
            assert!(error.to_string().contains("GITCASK_FORWARD_SECRET"));
            return Ok(());
        }
        config.validate()?;
        let parsed = Config::parse(
            r#"
[server]
auth_mode = "introspect_forwarded"
[auth.introspect]
url = "https://issuer.example/introspect"
secret_env = "CARGO_PKG_NAME"
"#,
        )?;
        assert_eq!(parsed.server.auth_mode, AuthMode::IntrospectForwarded);
        for url in [
            "",
            "http://issuer.example/i",
            "https://user:pass@issuer.example/i",
        ] {
            config.auth.introspect.url = url.into();
            assert!(config.validate().is_err());
        }
        config.auth.introspect.url = "https://issuer.example/i".into();
        for name in [
            "",
            "GITCASK_TEST_UNSET_COMBINED",
            "INVALID=NAME",
            "GITCASK_TEST_COMBINED_EMPTY",
            "GITCASK_TEST_COMBINED_INVALID",
        ] {
            config.auth.introspect.secret_env = name.into();
            assert!(config.validate().is_err());
        }
        config.auth.introspect.secret_env = "CARGO_PKG_NAME".into();
        config.auth.introspect.cache_ttl = Duration::from_secs(601);
        assert!(config.validate().is_err());
        config.auth.introspect.cache_ttl = Duration::ZERO;
        for timeout in [Duration::ZERO, Duration::from_secs(11)] {
            config.auth.introspect.timeout = timeout;
            assert!(config.validate().is_err());
        }
        Ok(())
    }

    #[test]
    fn authentication_urls_require_https_or_loopback() {
        for url in [
            "https://issuer.example/introspect",
            "http://localhost:1234/i",
            "http://127.0.0.1/i",
            "http://127.0.0.2:1234/i",
            "http://[::1]:1234/i",
        ] {
            assert!(secure_auth_url(url), "{url}");
        }
        for url in [
            "",
            "https://",
            "http://issuer.example/i",
            "ftp://localhost/i",
            "http://localhost:1234@evil.example/i",
            "https://user:secret@issuer.example/i",
            "http://127.0.0.1.example/i",
            "http://[::2]/i",
            "https://issuer.example/#fragment",
        ] {
            assert!(!secure_auth_url(url), "{url}");
        }
    }

    #[test]
    fn introspect_empty_secret_is_rejected() {
        const VARIABLE: &str = "GITCASK_TEST_INTROSPECT_EMPTY_38";
        if std::env::var_os(VARIABLE).is_some() {
            let mut config = Config::default();
            config.server.auth_mode = AuthMode::Introspect;
            config.auth.introspect.url = "https://issuer.example/i".into();
            config.auth.introspect.secret_env = VARIABLE.into();
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("secret_env")
            );
        } else {
            // A child owns its environment; no unsafe set_var in a threaded test.
            assert!(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "tests::introspect_empty_secret_is_rejected"])
                    .env(VARIABLE, "")
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
}
