# gitcask cross-crate contract

Context: **the original cross-crate interface contract (2026-08-18/19, written so eight owners could build the
crates in parallel)**, kept as the reference for names and shapes. Rule still in force: *extend, do not rename*
— a type or function listed here is relied on by another crate. **Where this file and the code disagree, the
code is right and this file is stale**; verify with `rg`/`cargo doc` before relying on a signature. Known
supersessions (2026-08-20 sweep): synchronization has refs-only and full-local levels
(`sync_refs`, `sync_refs_only`, and `sync_full`, `AGENTS.md §2.3`); the server router is
`AGENTS.md D15/D20/D26/D27`. Read when you touch a crate boundary; update
the relevant block when you extend one.

Shared interfaces between crates. Implement exactly these names/shapes; extend freely, do not rename.
Original owners (parallel batch): StoreS3, StoreCoord, GitEngine, Wal, Server, Cli.
Read `AGENTS.md` first (design §1–§2, decisions §3; the original layout/phases/config draft is the measurement log).

## Existing (do not rewrite; extend only)
- `gitcask-proto`: prost types from `proto/gitcask/v1/wal.proto` (Manifest, LogSegmentRef, LogEntry, PackRef,
  RefTransaction/RefUpdate, Checkpoint(+Ref), RefSnapshot/Ref, Lease); `keys::*`;
  `frame::{encode_entries,decode_entries}` (uvarint-framed log encoding); `time::*`.
- `gitcask-store`: `ObjectStore` trait (`Version` opaque CAS token, `ObjectMeta{key,size,version,last_modified}`,
  `GetOptions{if_none_match,if_match,range}`,
  `GetResult::{NotModified,Object}`, `PutMode::{Overwrite,Create,Update(Version)}`, `PutBody::{Bytes,Stream,File}`,
  `PutOptions`, `StoreError::{NotFound,PreconditionFailed{current},Retryable,InvalidArgument,Other}`,
  `ObjectStoreExt`, `Prefixed`, `memory::MemoryStore`, `util::{collect,once,file_stream,backoff,retry}`),
  modules `coord.rs`, `s3.rs`, and test-only `fault.rs` / `memory.rs`.
- `gitcask-config`: `Config` for gitcask.toml (+ `GITCASK__` env overrides, `PORT`).

## gitcask-git (owner: GitEngine)

```rust
pub struct RepoId { owner: String, name: String }
// FromStr("owner/name" | "owner/name.git"), Display "owner/name". Validation: each part ASCII [A-Za-z0-9._-],
// no leading '.', not "..", 1..=100 chars. fn owner(), name(), store_prefix() (gitcask_proto::keys::repo_prefix),
// local_dir(root:&Path)->PathBuf (= root/owner/name.git).
pub enum ObjectFormat { Sha1, Sha256 } // From<gitcask_config::ObjectFormat>, <-> gix_hash::Kind, as_str()

/// Bare git repo on local disk in standard layout (objects/pack/*.{pack,idx}, loose refs + packed-refs, HEAD,
/// config with repositoryformatversion / extensions.objectformat) readable by gix AND upstream git.
/// Clone-able handle (Arc inside), thread-safe.
pub struct LocalRepo;
impl LocalRepo {
  pub fn init(root: &Path, id: &RepoId, format: ObjectFormat) -> Result<Self, GitError>;
  pub fn open(root: &Path, id: &RepoId) -> Result<Option<Self>, GitError>;
  pub fn id(&self) -> &RepoId; pub fn path(&self) -> &Path; pub fn object_format(&self) -> ObjectFormat;
  pub fn gix(&self) -> gix::Repository;          // per-thread handle from shared ThreadSafeRepository
  pub fn refresh(&self) -> Result<(), GitError>;  // re-read odb/refs after pack/ref changes

  // ---- packs
  /// = git index-pack: stream in, write objects/pack/pack-<checksum>.{pack,idx}; thin packs resolved against
  /// the odb (--fix-thin); verify checksum; opts.fsck => object-level validation. Empty input => Ok(None).
  pub async fn ingest_pack<R: tokio::io::AsyncRead + Unpin + Send + 'static>(&self, pack: R, opts: IngestOptions)
      -> Result<Option<IngestedPack>, GitError>;
  pub struct IngestOptions { pub fsck: bool, pub max_bytes: Option<u64>, pub thin: bool }
  pub struct IngestedPack { pub checksum: gix_hash::ObjectId, pub pack_path: PathBuf, pub idx_path: PathBuf,
      pub pack_size: u64, pub idx_size: u64, pub object_count: u64 }
  /// Atomically move downloaded files into objects/pack/ (rename), then refresh.
  pub async fn install_pack(&self, pack: &Path, idx: &Path, extra: &[PathBuf]) -> Result<(), GitError>;
  /// Delete .pack/.idx/.rev/.bitmap. Caller guarantees no readers (wal holds a lock).
  pub fn remove_pack(&self, checksum: &gix_hash::oid) -> Result<(), GitError>;
  pub fn packs(&self) -> Result<Vec<PackInfo>, GitError>;
  pub struct PackInfo { pub checksum: gix_hash::ObjectId, pub pack_size: u64, pub idx_size: u64,
      pub object_count: u64, pub has_rev: bool, pub has_bitmap: bool }
  pub fn pack_path(&self, checksum: &gix_hash::oid) -> PathBuf; // objects/pack/pack-<hex>.pack (idx: set_extension)

  // ---- refs
  /// All refs sorted by name incl. peeled tags + HEAD symbolic target. `From` both ways with
  /// gitcask_proto::v1::RefSnapshot.
  pub fn refs(&self) -> Result<RefSnapshotData, GitError>;
  /// Atomic all-or-nothing. check_old => verify old_oid (zero = must not exist). Supports HEAD symbolic update.
  /// Error GitError::RefConflict{name, expected, actual}.
  pub fn apply_ref_txn(&self, txn: &gitcask_proto::v1::RefTransaction, check_old: bool) -> Result<(), GitError>;
  /// Replace ALL refs + HEAD (write packed-refs directly; must be fast for 500k refs).
  pub fn load_ref_snapshot(&self, snap: &gitcask_proto::v1::RefSnapshot) -> Result<(), GitError>;
  pub fn pack_refs(&self) -> Result<(), GitError>;

  // ---- objects
  pub fn has_object(&self, oid: &gix_hash::oid) -> bool;
  /// Every object reachable from tips exists. `stop_at_existing_refs` => stop at objects reachable from
  /// current refs (rev-list --objects <tips> --not --all). Error GitError::MissingObject{oid}.
  pub fn check_connectivity(&self, tips: &[gix_hash::ObjectId], stop_at_existing_refs: bool) -> Result<(), GitError>;

  // ---- protocol, server side
  /// Raw passthrough: spawns `git upload-pack --stateless-rpc` with `GIT_PROTOCOL` set.
  pub async fn upload_pack_raw<R, W>(&self, protocol: Protocol, body: R, out: W) -> Result<(), GitError>;
  /// v2 ls-refs from the ref snapshot; efficient prefix filtering.
  pub fn ls_refs(&self, args: &LsRefsArgs) -> Result<Vec<LsRefsLine>, GitError>;
  pub struct LsRefsArgs { pub ref_prefixes: Vec<String>, pub symrefs: bool, pub peel: bool, pub unborn: bool }
  /// v0 advertisement with capabilities.
  pub fn advertise_refs_v0(&self, service: Service, out: &mut Vec<u8>) -> Result<(), GitError>;
  pub enum Service { UploadPack, ReceivePack }  // FromStr("git-upload-pack"|"git-receive-pack")

  // ---- upstream git helpers
  pub async fn git(&self, args: &[&str]) -> Result<std::process::Output, GitError>; // cwd=repo, GIT_DIR set
  pub async fn repack(&self, opts: RepackOptions) -> Result<RepackResult, GitError>;
  pub struct RepackOptions { pub mode: RepackMode /* Geometric{factor} | Full */, pub write_bitmap: bool,
      pub write_midx: bool, pub keep: Vec<gix_hash::ObjectId> }
  pub struct RepackResult { pub new_packs: Vec<PackInfo>, pub removed: Vec<gix_hash::ObjectId> }
}

pub mod pkt;      // pkt-line read/write, flush/delim/response-end, sideband encode; Protocol::{V0,V2} from
                  // GIT_PROTOCOL header; command/arg parsing for v2 (ls-refs, fetch, object-info)
pub mod receive;  // parse receive-pack request: caps + commands ("old new refname\0caps"), push-options,
                  // => (gitcask_proto::v1::RefTransaction, ReceiveCaps{report_status_v2, side_band_64k,
                  // atomic, quiet, push_options, agent, object_format}); pack bytes follow in the same body.
                  // `report_status(caps, unpack: Result, per_ref: &[(name, Result<(),String>)], out)` writer
                  // producing report-status(-v2), sideband-framed when requested.
pub enum GitError { Io, Gix(Box<dyn Error+Send+Sync>), Pack, RefConflict{name,expected,actual}, MissingObject{oid},
                    Fsck(String), Subprocess{cmd,status,stderr}, InvalidInput(String), Protocol(String) }
```

## gitcask-store::coord (owner: StoreCoord)

```rust
/// Generic read-modify-write CAS loop on a protobuf object. `f(None)` when absent. Returning `None` from `f`
/// aborts with Ok(None). Retries on PreconditionFailed (re-reading) up to `max_retries`, on Retryable with
/// backoff. Returns the written meta + value.
pub async fn cas_update<T: prost::Message + Default, F>(store: &dyn ObjectStore, key: &str, max_retries: u32, f: F)
    -> Result<Option<(ObjectMeta, T)>, CoordError>
  where F: FnMut(Option<&T>) -> Result<Option<T>, CoordError>;
/// Read a protobuf object with its version. Ok(None) if absent.
pub async fn get_message<T: prost::Message + Default>(store: &dyn ObjectStore, key: &str)
    -> Result<Option<(ObjectMeta, T)>, CoordError>;
pub async fn get_message_if_changed<T>(store, key, known: &Version) -> Result<Option<(ObjectMeta, T)>, CoordError>;

/// Lease = gitcask_proto::v1::Lease at `key`, acquired by Create or by Update over an expired lease.
pub struct LeaseGuard; // holds store handle, key, holder id, current Version; Drop => best-effort release
impl LeaseGuard {
  pub async fn heartbeat(&mut self, ttl: Duration) -> Result<(), CoordError>;      // CAS-extend expires_at
  pub async fn release(self) -> Result<(), CoordError>;                            // CAS delete
  pub fn spawn_heartbeat(self: Arc<Mutex<Self>>, every: Duration, ttl: Duration) -> tokio::task::JoinHandle<()>;
  pub fn holder(&self) -> &str; pub fn expires_at(&self) -> SystemTime;
}
pub async fn try_acquire(store: DynStore, key: &str, holder: &str, purpose: &str, ttl: Duration)
    -> Result<Option<LeaseGuard>, CoordError>;   // None = held by someone else and not expired
pub async fn acquire(store, key, holder, purpose, ttl, wait_up_to: Duration) -> Result<Option<LeaseGuard>, CoordError>;
pub fn instance_id() -> &'static str; // explicit instance name/id, hostname+pid, or uuid; computed once
pub enum CoordError { Store(StoreError), Decode(prost::DecodeError), Aborted, RetriesExhausted{key, attempts}, Other }
```

## gitcask-store backend (owner: StoreS3)

```rust
// s3.rs
pub struct S3Store; impl S3Store { pub async fn new(cfg: &gitcask_config::StoreConfig) -> anyhow::Result<Self>; }
// lib.rs
pub async fn open_store(cfg: &gitcask_config::Config) -> anyhow::Result<DynStore>; // by cfg.store.backend, applies Prefixed(cfg.store_prefix())
```
Contract tests: `crates/gitcask-store/tests/contract.rs` with a `run_contract(store: DynStore)` suite executed for
memory always, and for S3 when `GITCASK_TEST_S3_ENDPOINT` is set (bucket `GITCASK_TEST_BUCKET`, default
"gitcask-test"). The memory store and fault wrapper are exposed only with the `testing` feature.

## gitcask-wal (owner: Wal)

```rust
pub struct Registry;   // one per process: DynStore + Arc<Config> + cache_root; DashMap<RepoId, Arc<RepoHandle>>
impl Registry {
  pub fn new(store: DynStore, cfg: Arc<gitcask_config::Config>) -> Arc<Self>;
  /// Open existing (materialize local copy lazily). Err(WalError::NotFound) if manifest.pb absent.
  pub async fn open(&self, id: &RepoId) -> Result<Arc<RepoHandle>, WalError>;
  /// CAS-create manifest.pb (PutMode::Create). Err(WalError::AlreadyExists).
  pub async fn create(&self, id: &RepoId, format: ObjectFormat) -> Result<Arc<RepoHandle>, WalError>;
  pub async fn open_or_create(&self, id: &RepoId, format: ObjectFormat) -> Result<Arc<RepoHandle>, WalError>;
  /// Repositories materialized by this process (cache/event sweep inspection only).
  pub fn cached_repos(&self) -> Vec<RepoId>;
  pub fn store(&self) -> &DynStore; pub fn config(&self) -> &Arc<Config>;
  /// Disk cache maintenance: evict idle repos and relieve disk pressure.
  pub async fn evict_idle(&self) -> Result<EvictReport, WalError>;
}
pub struct RepoHandle;
impl RepoHandle {
  pub fn id(&self) -> &RepoId;
  pub fn local(&self) -> &LocalRepo;
  pub fn store(&self) -> &Prefixed;                       // repo-scoped
  pub fn manifest(&self) -> Arc<gitcask_proto::v1::Manifest>;   // last known
  pub fn manifest_version(&self) -> Option<Version>;
  /// Freshness check (conditional GET on manifest.pb; honors wal.freshness_ttl) + catch-up (download new
  /// packs, apply log entries after our seq, apply COMPACT: install new pack, remove superseded). Returns a
  /// read guard; while any guard is alive no pack is removed locally. Every request calls this first.
  pub async fn sync_full(&self) -> Result<ReadGuard<'_>, WalError>;
  pub async fn sync_refs(&self) -> Result<ReadGuard<'_>, WalError>;
  pub async fn sync_refs_only(&self) -> Result<ReadGuard<'_>, WalError>;
  pub async fn try_compaction_lease(&self) -> Result<Option<LeaseGuard>, WalError>;
  pub async fn try_checkpoint_lease(&self) -> Result<Option<LeaseGuard>, WalError>;
  /// Force full re-materialize from store (repair).
  pub async fn rematerialize(&self) -> Result<(), WalError>;
  /// Publish a push. `pack` was produced by LocalRepo::ingest_pack on this handle's local repo (already on
  /// disk). Steps: upload pack+idx to wal/<sha>.{pack,idx} (skip if exists) ‖ verify txn old values against
  /// synced refs; then CAS: append LogEntry to log (new segment object per batch on regional buckets),
  /// cas_update manifest (head_seq+1, packs+=, log_segments+=); on PreconditionFailed: re-sync, re-verify
  /// old values (conflict per ref → whole push rejected unless !atomic and per-ref reporting), retry.
  /// Then apply refs locally. Coalesces concurrent publishes on this handle (wal.batch_window/max_batch).
  pub async fn publish_push(&self, pack: Option<IngestedPack>, txn: RefTransaction, meta: HashMap<String,String>)
      -> Result<PublishResult, WalError>;
  pub struct PublishResult { pub seq: u64, pub per_ref: Vec<(String, Result<(), RefError>)> }
  pub async fn publish_ref_update(&self, txn: RefTransaction, meta) -> Result<PublishResult, WalError>;
  /// COMPACT entry: new pack (already local, e.g. from LocalRepo::repack) superseding `supersedes`.
  pub async fn publish_compact(&self, new_pack: PackInfo, supersedes: Vec<gix_hash::ObjectId>, tier: u32)
      -> Result<u64, WalError>;
  /// Write checkpoint at current head (refs snapshot + pack set), then CAS manifest (checkpoint=, min_seq=,
  /// log_segments trimmed). Idempotent.
  pub async fn write_checkpoint(&self) -> Result<CheckpointRef, WalError>;
  /// Read log entries [from_seq, to_seq] from the store (provenance/rewind tooling).
  pub async fn read_log(&self, from_seq: u64, to_seq: Option<u64>) -> Result<Vec<LogEntry>, WalError>;
  pub fn last_access(&self) -> Instant;  pub fn touch(&self);
}
pub enum WalError { NotFound, AlreadyExists, Store(StoreError),
                    Coord(CoordError), Git(GitError), Publish{msg,retryable}, Corrupt(String), Retry{attempts},
                    Io(std::io::Error) }
// WalError::is_retryable() preserves transient store failures across publish batching and crate boundaries.
pub enum RefError { NonFastForward, Conflict{expected,actual}, Rejected(String), Missing }
```
`TaskRecord`, `TaskOutcome`, and `Progress` implement both `Serialize` and `ToSchema`; the server's task JSON
and OpenAPI components are generated from the same cross-crate types.

## gitcask-server (owner: Server)

```rust
pub struct AppState { pub cfg: Arc<Config>, pub store: DynStore, pub registry: Arc<gitcask_wal::Registry>,
                      pub auth: Arc<auth::Authenticator> }
pub fn router(state: Arc<AppState>) -> axum::Router;
pub struct auth::Authenticator;
impl auth::Authenticator {
  pub async fn authenticate(&self, headers: &HeaderMap) -> Result<Principal, AuthError>;
  pub async fn require_read(&self, headers: &HeaderMap, owner: &str, repo: &str) -> Result<Principal, AuthError>;
  pub async fn require_write(&self, headers: &HeaderMap, owner: &str, repo: &str) -> Result<Principal, AuthError>;
  pub async fn require_admin(&self, headers: &HeaderMap, owner: &str, repo: &str) -> Result<Principal, AuthError>;
}
pub fn auth::mint_token(private_key_pem: &str, issuer: &str, audience: Option<&str>, principal: &str,
                        scopes: &[String], ttl: Duration) -> anyhow::Result<String>;
pub fn auth::generate_key_pair_pem() -> anyhow::Result<(String, String)>;
/// Bind, serve (HTTP/1.1 + h2c), graceful shutdown on SIGTERM/SIGINT/`shutdown` future.
pub async fn serve(state: Arc<AppState>, shutdown: impl Future<Output=()> + Send) -> anyhow::Result<()>;
// gc::due/collect: lowest-priority per-repo maintenance; one repo-prefix LIST,
// manifest-generation revalidation, then version-conditional deletes under leases/gc.pb.
// Routes (all under /{owner}/{repo}[.git]):
//   GET  /info/refs?service=git-upload-pack|git-receive-pack   (v0 advert or v2 capability advert per Git-Protocol)
//   POST /git-upload-pack   POST /git-receive-pack   (Content-Encoding: gzip supported; streaming both ways)
//   GET  /HEAD  GET /objects/info/packs (404 unless dumb enabled)
//   POST /info/lfs/objects/batch  PUT/GET /info/lfs/objects/{oid}  POST /info/lfs/verify
//   PUT  /  (create repo, write permission)   DELETE / (admin permission)
// Non-repo: GET /healthz /readyz /metrics
// Auth: EdDSA JWT from Git Basic password / API Bearer is verified against public PEM or cached JWKS and
// repository scopes are checked by require_read/write/admin. Forwarded mode remains optional. Endpoints
// synchronize refs only or the complete local pack set (AGENTS.md §2.3).
```


## gitcask-cli (owner: Cli)
`gitcask --config gitcask.toml <cmd>`: `serve` | `compact owner/name [--once]` |
`repo create|info` | `wal pending|ls|show|materialize --at-seq` | `synth --out DIR --size s|m|l [--commits N --files M]`
| `import --from GITDIR owner/name` | `token keygen|mint`. Also `Containerfile`, `compose.yaml` (rustfs +
gitcask), `justfile`, `gitcask.example.toml`, `tests/e2e.sh` (real git vs. server on memory store and on rustfs).
