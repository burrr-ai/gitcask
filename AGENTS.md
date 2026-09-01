# gitcask — architecture and operating manual for contributors (humans and agents)

Context: **everyone, first.** Read this before touching the repository: the constraints the design answers
(§1), how the WAL works (§2), the principles a PR is judged against (§3), every design decision in force (§4),
and the working rules (§5). Reading order starts at `GOAL.md`; §0 maps every other document.

> **No backwards compatibility (pre-1.0).** We owe nothing to previous shapes of this system. Do not keep
> aliases, fallbacks, shims, deprecated routes, old config keys, old proto fields or migration code. When a
> decision changes the shape, **delete the old shape in the same change**: routes, clients, tests, docs, config
> keys. Data in the bucket is the one exception — WAL/manifest/log formats stay append-only and replayable,
> because the bucket is the repository; everything else is disposable. If you find yourself writing "still
> accepted for …" or "legacy", stop and remove the thing instead.

gitcask serves git over smart HTTP (v0/v2), receive-pack, upload-pack, LFS and a JSON API,
written in Rust, from disposable hosts whose only durable state is an object-store bucket. The design follows Cursor's
*Git at any scale* (Continuity) as implemented by walgit, reshaped for platforms with many small repositories and
identity and metadata outside gitcask; comwit is the first such user. `README.md` tells the story, `GOAL.md` the
target, `docs/DIRECTION.md` what
this fork changed and the operating decisions (D1–D14 there are *operating* decisions, distinct from the design
decisions numbered in §4 here); this file keeps the rules.

## 0. Document map (one home per fact — link, don't duplicate)

| Doc | Who / when to read it |
|---|---|
| `GOAL.md` | Everyone, first. What gitcask is for, the acceptance table, what we do not optimise for. |
| `docs/DIRECTION.md` | Everyone. Why this fork differs from walgit, what was removed and why, the operating decisions (D1–D14). |
| `docs/PRODUCT.md` | Everyone. The product boundary: core vs platform, OSS vs cloud, the two judging rules, and what we will not build — with the reason for each. |
| `scripts/smoke.sh`, `scripts/clippy-count.sh` | The end-to-end smoke test against rustfs and the deterministic clippy counter. |
| `AGENTS.md` (this) | Everyone. Constraints §1, WAL §2, principles §3, decisions §4, working rules §5. |
| `README.md` | The introduction: why (the Cursor lineage), what it does, how it works briefly, running it, invariants. |
| `docs/ROUNDTRIPS.md` | **Anyone touching a protocol that talks to the bucket** (publish, sync, checkpoints, compaction/leases, store backends). Round trips are the cost model; correct is not sufficient. |
| `docs/LFS.md` | Anyone touching LFS (`lfs.rs`) or importing a repository whose LFS history lives elsewhere. |
| `docs/INTEGRITY.md` | Anyone touching import, the maintainer's `fsck` unit, or seeing `connectivity: missing object` on a push. |
| `docs/EVENTS.md` | Anyone changing WAL-derived ref events, the webhook bridge, consumer semantics or event cursors. |
| `docs/OPERATIONS.md` | Operators and on-call responders. Metrics, symptom-first diagnosis, capacity, incidents, recovery and routine checks. |
| `docs/CONTRACT.md` | When you touch a crate boundary. The cross-crate contract; *extend, don't rename*; code wins where they differ. |
| `docs/reference/cursor-git-at-any-scale.md` | Pointer + key excerpts of the source design (the full post is Cursor's copyright). Read the post once before touching WAL/publish/sync. |
| `gitcask.example.toml` | Every config key with its default and a comment. Change it with the code. |
| `gitcask.standalone.toml` | The one-machine shape: one JWT-verifying `gitcask-server` on :8080 → rustfs. |
| `deploy/nginx.conf.example` | Optional public TLS and `X-Accel-Redirect` byte offload in front of gitcask. |
| `Containerfile` | An OCI image. |

---

## 1. Constraints we design for

### 1.1 The machine: ephemeral, shared-nothing
- **Instances are ephemeral and shared-nothing**: no stable identity, no node-to-node networking, no gossip.
  Every serving host can perform a repository's refs and object work, including during a deploy.
- **CPU may be throttled between requests** on serverless platforms; background work must stay off the serving
  runtime and run on the bounded bulk runtime.
- **Object store facts**: ~60–80 ms per GET, ~100 MB/s per connection (stripe for more), conditional GET ~15 ms;
  same-object overwrite is serialized (~1 write/s) — a single CAS'd object is a throughput cap.
### 1.2 What follows
- **An instance must become useful in seconds**, cold: refs in < 1 s (one manifest GET + snapshot + tail).
- **Everything an instance computes that is immutable is cached for everyone**: in-process LRU and, where a
  second instance would otherwise recompute, the bucket itself (`cache/api/v1/*.json`,
  `cache/archive/v1/*`). Wiping every instance loses nothing but warmth.
- **No silent waiting.** Long work is a *task* (id, log, lock, attachable stream) and is narrated to the
  client: SSE envelope for API clients, sideband band-2 lines for git. "Cloning into… and then nothing" is a bug.

### 1.3 Security contract (`Config::validate` fails closed)
- Three auth modes (`server.auth_mode`): **`none`** (everyone is `anon` with write and admin — `validate` refuses
  unless `server.listen` is loopback), **`jwt`** (the normal standalone/public path), and **`forwarded`**.
  JWT mode accepts EdDSA tokens from a Git Basic password or API Bearer header, verifies `[auth.jwt]` public-key
  PEM or cached JWKS plus issuer/audience/times, and applies `<owner>/<repo>:read|write|admin` scopes. Scope
  misses are 404; missing/invalid credentials are 401 with `WWW-Authenticate: Basic realm="gitcask"`. Client
  `X-Gitcask-Principal`/`-Write`/`-Admin` headers are ignored. In forwarded mode the authenticating proxy supplies
  `X-Gitcask-Principal`; `X-Gitcask-Write: 1` and `X-Gitcask-Admin: 1` grant those permissions. If
  `GITCASK_FORWARD_SECRET` is set, `X-Gitcask-Forward-Secret` must match it. `Authorization` is ignored.
- `/healthz` and `/readyz` are open at the application. Everything else requires valid credentials.
- **An edge announces byte offload, per request, in `X-Gitcask-Capabilities`** (D39): `accel-redirect` means
  static bytes may be served by `X-Accel-Redirect`, honoured only when
  `server.accel_redirect = true` **and the TCP peer is loopback**). Hit directly, nothing is assumed.
- Tests never write the user's global git config (private `GIT_CONFIG_GLOBAL`).

### 1.4 Requirements (the bar)
- Full git surface: smart HTTP v0/v2, ls-refs with prefixes, fetch with filter/shallow/deepen/sideband-all,
  receive-pack (atomic, delete, tags, push options, report-status-v2),
  LFS batch/basic transfer, `<owner>/<repo>` namespaces. Every read on every instance is as fresh as a fetch:
  push acknowledged ⇒ the next request anywhere sees it.
- Cost must not scale with ref count on any hot path (O(1) `refs`, O(k) `resolve`, paged ref lists, prefix
  filtered advertisements).

---

## 2. The WAL — source of truth, and how we squeeze value out of it

### 2.1 Objects and the commit point (`repos/<owner>/<repo>/…`)
| Object | Role |
|---|---|
| `manifest.pb` | Tiny, **CAS-rewritten**: `head_seq`, live pack set `PackRef[]` (checksum, sizes, tier, has_rev/bitmap), log segments, checkpoint pointer, `revision`, `updated_at`. **The linearization point.** Nothing is visible before its CAS; everything after is idempotent and replayable. |
| `log/<first_seq>.pb` | Immutable, uvarint-framed `LogEntry` frames: PUSH / REF_UPDATE (ref transaction + pack pointer), COMPACT (new pack, `supersedes[]`). Strictly increasing `seq`. One small object per publish batch. |
| `wal/<checksum>.pack/.idx/.rev/.bitmap/.commit-graph` | Immutable packs, content-addressed by pack checksum: push packs (tier 0), compaction outputs (tier 1), plus the side-files a reader needs. |
| `checkpoints/<seq>/checkpoint.pb`, `refs.pb` | Folded state at `seq`: live pack set + full `RefSnapshot`. Cold start = snapshot + tail, never full replay. |
| `leases/<name>.pb` | CAS lease with TTL heartbeat, such as `compact`. The only cross-instance mutex. |
| `cache/api/v1/<sha1>.json` | Shared render cache of immutable web API answers. |
| `cache/archive/v1/<sha1>.<format>` | Shared immutable prefix-free `git archive` result (one per commit/format), served through the static-byte path. Free-form `?prefix=` variants are never stored here. |
| `fsck.pb` | Last connectivity audit (`FsckReport`), written by the maintainer's `fsck` unit and exported as a metric (`docs/INTEGRITY.md`). |
| `gc.pb` | Last completed bucket-GC compaction/checkpoint cursors (`GcState`); overwritten after conditional deletes finish, safe to lose and recompute (D43). |
| `events/cursor.json` | Durable acknowledged WAL sequence of the events bridge; advanced only after the webhook acknowledged (D32). |
| `lfs/objects/<aa>/<bb>/<oid>` | LFS objects (sha256-addressed, immutable). |
| `pending/<owner>/<repo>` (bucket root) | Empty marker written after every successful manifest CAS; the maintainer's work queue (D40). |
Schema `crates/gitcask-proto/proto/gitcask/v1/wal.proto`; the S3 backend and test-only in-memory store share one
contract suite (`crates/gitcask-store/tests/contract.rs`, incl. compose).

### 2.2 Write path
receive-pack (ours, `gitcask-git/src/receive.rs`) → index the pack locally (`git index-pack --stdin --fix-thin
--keep --rev-index --threads=0`, `--fsck-objects` when `wal.fsck_objects`) in a per-ingest scratch git dir (a
rejected push leaves nothing behind) → connectivity per config (`spawn_blocking`) →
`pack PUT ∥ idx PUT ∥ log PUT` → **manifest CAS** (group commit per repo per instance,
`wal.batch_window`) → commit local ref txn → `ok` to the client → best-effort `pending/<o>/<r>` marker PUT (a
failed marker never fails the push). On 412: refetch, re-validate every old value
(moved ref ⇒ `ng`), re-seq, retry with jittered backoff. Publish is CAS-safe for concurrent writers by
construction. Never ACK before the bucket ACKs.

### 2.3 Read path — sync levels (`RepoHandle::sync_*`, `gitcask-wal/src/handle.rs`, `sync.rs`)
Every request: conditional GET of `manifest.pb` (skippable for `wal.freshness_ttl`) → 304 serve / 200 apply.
| Level | Brings | Used by |
|---|---|---|
| **Refs** | checkpoint `RefSnapshot` + every log entry's ref txn → `packed-refs`. No packs. | `info/refs`, `ls-refs`, web `refs`/`resolve`/overview, read_log |
| **Full** (`sync_full()`) | Refs + every live pack local (striped parallel downloads) | upload-pack, receive-pack, web object endpoints, compaction |
Long syncs register as `materialize` tasks and stream progress; pack work runs on the **bulk
runtime** and never takes the refs phase's lock (D19). Local disk uses idle and watermark eviction.

### 2.4 Getting the most out of the WAL (the strategies)
- **Refs-first everything.** Ref advertisement, peeled tags (`RefUpdate.new_peeled` recorded by the writer so
  replicas advertise `^{}` without objects), web `refs`/`resolve` — all from snapshot + tail, no objects.
- **Side-files published with packs**: `.idx`, `.rev`, `.bitmap`, and split commit-graph layers. Every pack writer produces `.rev` (`pack.writeReverseIndex`; git
  ≥ 2.47 on the server); a published pack ≥ 250 k objects without one gets it from the maintainer (`rev-index`
  unit) — without it git rebuilds the reverse index in memory on every `pack-objects` (2.85 s per fetch on a
  60 M-object repository).
- **Checkpoints fold the log** when `wal.snapshot_every_entries` OR `wal.checkpoint_interval` OR
  `wal.checkpoint_tail_bytes` fires — the checkpoint is the unit of serving state. Writing one is refs-level work
  (`sync_refs_only`, no packs downloaded), evaluated only for repositories with a pending marker.
- **Provenance for free**: every push and repack is a log entry; `gitcask wal ls|show|materialize --at-seq`.
- **Never LIST on a hot path**; 404s are free; probe, don't list. Immutable objects get
  `Cache-Control: public, max-age=31536000, immutable` + strong ETag + Range everywhere (D10 static contract).

### 2.5 Compaction (WAL + git), leader by lease
- Maintainers run **geometric** folding of fresh packs (tier 0 → tier 1, `git repack -d --geometric
  --write-midx`) under `leases/compact.pb`; the result is a COMPACT entry; followers download the new pack and
  drop superseded ones after in-flight readers finish. Triggers: `compaction.trigger_packs`, `trigger_bytes` —
  and at least **two** fresh packs (one pack folds into itself).
- Superseded packs are retained `compaction.retention_superseded` (provenance window) then GC'd.

### 2.5b Self-healing by construction
Everything the maintainer produces — checkpoints, compactions and retention — is a **pure function
of (config, WAL state)**. The maintainer does not run schedules; for each repository it computes the *desired
state* and performs **one bounded unit of the most important missing work** at a time (checkpoint → compaction →
rev-index → fsck audit → bucket GC), as a task, under a lease, until the repository is idle. A deleted or corrupt artefact is
"missing" and rebuilt identically; config changes take effect by re-planning; there are no one-off backfill
scripts.

**Which repositories** (D40): never all of them. A pass lists `pending/` (at most `maintenance.max_repos_per_pass`
markers, with their versions), works each marked repository to idle, then deletes its marker conditionally on the
listed version — a push that arrived meanwhile rewrote the marker, so the delete fails and the repository is seen
again next pass. A repository nobody pushes to is never visited; time-based triggers (checkpoint age, fsck
interval) apply only to marked repositories. Cost is proportional to pushes, not to repository count. The only
other periodic LIST is `maintain/` once per 10 minutes; it scales with live instance count and expires stale
maintainer heartbeats.


### 2.6 Tasks, progress, narration (`gitcask-wal/src/tasks.rs`, `crates/gitcask-server/src/sse.rs`, `smart.rs`)
Any long work = a task: unique id, per-instance log (`GET …/tasks`), `(repo, kind)` lock (a second start joins),
replayable packet stream (`GET …/tasks/{id}`). Packets: `notice`, `progress {label,done,total?,unit,percent?}`,
`task`, terminal `result` | `error`. Web: any JSON endpoint that cannot answer immediately streams the SSE
envelope when the client accepts `text/event-stream`; fast answers stay plain cacheable JSON. Git: v2 `fetch`
advertises `sideband-all` and narrates on band 2 (`remote: * …`). `no-progress` is honoured.

---
## 3. Principles (what a PR is judged against)

One idea: **the bucket is the repository; everything else is a cache or a reader of the log.** Ten consequences.
A change that feels natural on a conventional git host (a table, a queue, a cache server, a webhook from the push
path, a bigger disk) is usually a violation here. A violation means the principle is wrong — amend it with a
decision in §4 — or the PR is; never "fix later".

| # | Principle | The tell in a PR | The question to answer |
|---|---|---|---|
| **I** | **No state outside the object store.** Disk and memory are caches. | A database, Redis, SQLite, a file that must survive a restart, an env var that encodes data. | "If every instance is wiped now, what is lost?" — must be "warmth". |
| **II** | **The manifest CAS is the only commit point.** Immutable objects are never overwritten (`PutMode::Overwrite` only on the manifest, leases, fsck.pb, gc.pb, events/cursor, maintainer heartbeats, render cache). | A second "commit" (a flag file, a list update that makes data visible), an ACK before the bucket's. | "What does a client on another instance see between the PUT and the CAS?" — nothing new. |
| **III** | **Side effects are readers of the WAL, never steps of a write.** Events and notifications tail the log from a durable cursor. | A webhook/HTTP call from `receive.rs`, `publish.rs`, `smart.rs`. | "If this side effect fails, does the push?" — no. "Is it replayable from the cursor?" — yes. |
| **IV** | **Every read revalidates; there is no eventually.** | A cache that outlives the manifest's generation, a TTL invented for a repo-scoped answer, a read that skips `sync_*`. | "After `push` returns `ok`, can any instance serve the old state?" (`cargo test -p gitcask-server --test sim`). |
| **V** | **Packs are local; never a hard-coded host.** | An object path that skips full synchronization, a hostname in `crates/`. | "Has the full pack set been synchronized before this reads objects?" |
| **VI** | **Never block the async runtime; bulk bytes never share a lane with the control plane.** | `Command::new(...).output()` or `std::fs` big reads in an `async fn` outside `spawn_blocking`/the bulk runtime; a queued writer on `RepoHandle::rw` from an install path. | "On which thread does this run, and what holds `sync_mutex`/`rw` while it does?" (e2e `blocking_work_in_the_install_path_does_not_stall_requests`). |
| **VII** | **No LIST on a hot path; count the round trips.** | A `.list(` in request handling; a new GET "just to check"; a protocol change without a `docs/ROUNDTRIPS.md` row. | Before/after depth in the commit; the sim asserts request budgets (`FaultStore::stats().ops`). |
| **VIII** | **Standalone first; the edge announces, the app never assumes.** | Reading an `X-Gitcask-*` request header without checking `X-Gitcask-Capabilities`; a feature only testable behind nginx. | "Does this work on `gitcask.standalone.toml` with nothing in front?" |
| **IX** | **No silent waiting.** | A new op that blocks a handler, a loop without a task, a `sleep` a client would wait on. | "Where does the client see this taking time?" — a task kind, a progress packet or a band-2 line. |
| **X** | **Keep gitcask small.** Upstream `git` does git things; gix only where measured faster and correct; one config file, one auth story, one SDK; scope is `GOAL.md §4`. Proto append-only. | A new dependency without a why; a reimplementation of something `git` does; an alias/shim/"legacy" branch; a removed proto field. | "Which line of GOAL §4 is this for?" |

---

## 4. Decisions in force (append, never silently change)
- **D1** Rust, tokio + axum, HTTP/2 (h2c or ALPN), streaming both ways, gzip request bodies.
- **D2** gix in-process where it is correct and measured; upstream `git` for upload-pack and delta-compressing repack.
- **D3** `ObjectStore` trait with CAS version tokens, conditional GET, range, compose and edge-fetch
  capabilities. Production backend scope is governed by D44.
- **D4** protobuf on the wire and in the bucket; schema versioned, append-only.
- **D5** Repo identity `<owner>/<repo>[.git]`, prefix `repos/<o>/<r>/`, creation = CAS create of the manifest.
- **D6** Manifest CAS is the only commit point. **D7** No node identity, no elections; leases for exclusivity.
- **D8** `gitcask.toml` only (+ `GITCASK__` env overrides). **D9** One binary, roles by config (`serve`,
  `maintain`, `events`; `maintain` includes compaction).
- **D10** One static-serving code path for every immutable byte (ETag/304/If-Range/Range/416/HEAD/immutable;
  UI assets precompressed at build; store objects never compressed at request time).
- **D12** (**superseded by D46**) `server.auth_mode` was `none` | `forwarded`; a front proxy authenticated clients
  and sent principal/write/admin grants to gitcask.
- **D13** Long work is a task and is narrated (§2.6); no endpoint may block silently.
- **D14** (retired) No embedded web UI: gitcask is an API + git server; any UI lives in the calling platform.
- **D15** Repo-scoped API lives at `/{o}/{r}/api/…` (refs, resolve, tree, blob, commits, commit, compare,
  overview, ops, tasks). The browser lane is
  `/{o}/{r}/api-browser/…`; `/api/v1` is non-repository discovery. No aliases.
- **D19** **The serving runtime is untouchable.** (1) control-plane store objects (manifest, log, checkpoints,
  leases, render cache) do not queue behind bulk bytes; S3 bulk GETs use presigned HTTP while control-plane
  operations use the SDK path; (2) pack
  materialization never runs on the serving runtime (`sync::on_bulk_runtime`) and never queues as a writer on
  `RepoHandle::rw` — the refs phase needs only `sync_mutex`, pack removal is `try_write()` (a queued writer on a
  tokio RwLock blocks every new reader; one 24-minute clone once starved every info/refs for minutes). The bulk
  runtime has `cache.bulk_threads` async workers (default 2); blocking filesystem/git work uses its blocking pool.
- **D20** **One API, two lanes**: `/{o}/{r}/api/…` is the direct lane;
  `/{o}/{r}/api-browser/…` is the cross-origin browser lane (CORS only for `server.cors_origins`).
- **D23** (**authentication clause superseded by D46**) **An edge was load-bearing for authentication and optional for bytes.** A front proxy terminates TLS,
  authenticates clients, routes by `/<owner>/<repo>`, and may offload static bytes by `X-Accel-Redirect`
  — only when it injects `X-Gitcask-Capabilities: accel-redirect`; the app's answer supplies `X-Gitcask-Store-Url` /
  `-Authorization` / `-Key` and the edge slices and caches. `deploy/nginx.conf.example` is the reference; nothing
  in `crates/` knows a hostname.
- **D26** **Routing is by repo prefix, nothing else.** Everything whose path starts with `/<owner>/<repo>` (git
  smart HTTP, `.git` suffix, LFS, `/{o}/{r}/api*`) is one routing
  unit; an edge maps `^/<o>/<r>[./?]` → a host. "Which machine serves a repo" is decidable from the first path
  segments alone (the `Server` header shows it). **D27** Lanes are a segment *after* the repo prefix
  (`/api`, `/api-browser`); the prefix is still the only routing key.
- **D31** **Draining must not stop serving.** *Phase 1* (SIGTERM): the maintenance loop starts no new unit and
  the running unit is **interrupted at once** (the next pass redoes it) while the instance serves everything
  normally, `/readyz` 200; bounded 30 s. *Phase 2*:
  `/readyz` 503 + Retry-After, new fetch/push/LFS refused with 503 before any work, in-flight requests get
  `server.drain_timeout`, exit. Test `tests/drain.rs`.
- **D32** **Events are produced from the WAL by one small service, never by the push path** (`docs/EVENTS.md`).
  The **events bridge** (`roles=["events"]`) tails each repo's log from a durable per-repo cursor
  (`events/cursor.json`), converts committed PUSH/REF_UPDATE entries to `ref` events, POSTs each batch to
  `events.webhook_url` (JSON array; `X-Gitcask-Delivery` = sha1 of the body; `X-Gitcask-Signature: sha256=<HMAC>`
  when `webhook_secret` is set), advances the cursor: published iff durable, a crash loses nothing, lag =
  `head_seq − cursor`. Writers and the WAL crate contain no event code. Wake-ups: `POST /_events/notify` from a
  bucket notification (at-least-once) + a periodic sweep as backstop. Dedup key `(repo, seq, ref_name)`.
- **D40** **The maintainer is driven by pending markers, never by a scan** (2026-08-30, `docs/DIRECTION.md` D1). The
  publish path writes `pending/<owner>/<repo>` after the manifest CAS (best-effort, `Overwrite`); `run_pass` lists
  `pending/` oldest-first and works repositories concurrently; a repo that reaches compaction runs to idle in
  that pass, while a checkpoint-only repo yields after one unit with its marker intact. Completed markers are
  deleted with their listed versions. There is no
  `registry.list()`, no LIST of `repos/` and no per-repository HEAD on any periodic path. Repository listing and
  metadata belong to the calling platform's database.
- **D41** **Packs are always local** (2026-08-30). `SyncLevel` is `Refs` | `Full`; there is no remote reader, no
  bucket mount, no prewarm, no tier-2 base or history pack. A repository that does not fit the instance is a
  sizing problem, not a code path.
- **D42** **Transient store errors are retried in the store** (2026-08-30):
  5xx / 429 / throttling / connection failures on GET, HEAD, LIST, DELETE, multipart and unconditional PUT are
  retried with full-jitter backoff up to `store.max_retries`; conditional PUTs are never retried at that layer
  (an ambiguous success cannot be told apart from a lost write) — `coord::cas_update` owns CAS retry.
- **D43** **Bucket GC is a lowest-priority, per-repository WAL reader** (2026-08-31). A maintainer runs it only
  for a pending repository after a COMPACT or checkpoint newer than `gc.pb`, after fsck, under
  `leases/gc.pb`. It lists that repository prefix once, retains the current manifest pack set, packs referenced
  by WAL entries inside `compaction.retention_superseded`, and packs in retained checkpoints; then it
  revalidates the manifest generation and conditionally deletes superseded pack families, folded logs, and old
  checkpoint directories by their listed versions. From the same listing, it also conditionally deletes
  `cache/api/v1/` and `cache/archive/v1/` objects older than `cache.shared_retention`; these two prefixes are
  the exception to the otherwise out-of-scope repository-prefix orphans. The latest checkpoint and every checkpoint
  inside the retention window remain. `wal materialize --at-seq` discovers those retained checkpoints; a sequence whose
  inputs were collected fails explicitly as beyond retention. `gc.pb` advances only after all deletes, is not a
  commit point, and may be lost and recomputed. There is no LIST across repositories and no time-driven visit;
  repository-prefix orphans unrelated to a committed supersede/checkpoint (other than the two shared cache
  prefixes above) are out of scope.
- **D39** **gitcask is a standalone program; any deployment is packaging.** `gitcask-server --config x.toml` (a thin
  bin = `gitcask serve`; `gitcask` with no subcommand serves too) works on one machine against one bucket in
  loopback-only `none` mode. It serves HTTP/1.1 + h2c; the front proxy terminates public TLS. Byte offload is
  announced per request in `X-Gitcask-Capabilities`, never assumed. `cache.dir` defaults to `/tmp/gitcask`. A
  missing `--config` file is fatal (exit 2); `--config /dev/null` is the explicit defaults+env form.
- **D44** **S3 is the sole production object-store backend** (2026-08-31). The in-memory and fault stores are
  test-only behind the `testing` feature. GCS-specific configuration, dependencies and protocol branches were
  removed; another production backend must justify and implement the complete store contract before returning.
- **D45** (**superseded by D46**) **Authentication was a stateless, separate gate** (2026-09-01). `gitcask-gate` was the public process;
  gitcask remains on its unchanged `forwarded` contract. Mint mode issues short-lived HS256 JWTs carrying only
  an opaque principal and repository scopes; static mode is configuration-only for local play and CI. The gate
  stores no identities, sessions, revocations or usage history, strips every client `X-Gitcask-*` header, and
  streams all bodies except the bounded LFS batch JSON. It shares `gitcask.toml` with the server. D9's one-binary
  rule applies to repository roles; the authentication boundary is intentionally a second process.
- **D46** **gitcask verifies asymmetric tokens itself; issuance and identity stay outside** (2026-09-01).
  `server.auth_mode` is `none` | `jwt` | `forwarded`; `jwt` is the one-process standalone/public path. Only
  EdDSA (Ed25519) is accepted. The issuer keeps the private key; gitcask has a public-key PEM or cached JWKS,
  refreshes JWKS only on `kid` miss, and retains the last successful set on refresh failure. Claims are opaque
  `sub`, repository `scopes`, `exp`, `iat`, `jti` (plus configured `iss`/optional `aud`; `nbf` is honoured).
  Git sends the token as the Basic password and APIs use Bearer. Existing handler `require_read`/`require_write`/
  `require_admin` calls are the sole path-permission table; scope misses return 404. There is no issuing endpoint:
  platforms sign the format themselves and self-hosters use offline `gitcask token keygen|mint`. The gate crate,
  HS256/static credentials, shared forward secret in the standard deployment, and second process are deleted.
  `forwarded` remains only for deployments that already have a trusted IdP proxy. gitcask stores no users,
  sessions, revocations, or token-use history. An edge terminates TLS and may offload bytes, but is not required
  for authentication.

Decision identifiers are stable; gaps in the numbering are intentional.

---

## 5. Working rules

- **No backwards compatibility (pre-1.0, banner at top):** change the shape and delete the old one in the same
  commit — no aliases, shims, deprecated routes/keys/fields. Only bucket formats (WAL, manifest, log, checkpoint
  protos) stay append-only/replayable.
- Keep this file current: append decisions with a number and a date; never delete history, replace it with the
  decision that superseded it.
- **Never block the async runtime**: no blocking git/fs work (repack, midx, commit-graph, gix reopen, large
  reads/copies) on a tokio worker — `spawn_blocking`; every `refresh()` on an async path is `refresh_async()`. Pack
  materialization runs on the **bulk runtime** (`sync::on_bulk_runtime`). The runtime watchdog logs "async runtime
  stalled" with `inflight` and `tasks_running`: `inflight = 0` at a late tick ⇒ the platform paused the process,
  `inflight > 0` ⇒ a real stall — look at `lock_wait_max_ms` and `gitcask_lock_wait_seconds{lock}`. Bulk bytes never
  queue on the serving runtime; S3 bulk GETs use their own presigned-HTTP path.
- **Correct is not sufficient.** Every protocol change (publish, sync, leases, checkpoints) is also
  judged on critical-path round trips against the bucket — read `docs/ROUNDTRIPS.md`, update its budget table, put
  before/after depth in the commit, keep verification on the failure path, assert request budgets in the sim.
- **Standalone first (D39, D46):** repository features and JWT authentication work by hitting one gitcask process
  directly with no external edge. Bytes are streamed by gitcask. Anything another
  edge takes over is announced per request in `X-Gitcask-Capabilities`; never infer an edge from config, never
  hardcode a hostname in `crates/`.
- **S3 is the production store.** Every store feature runs against the S3 contract suite (`just test-s3`
  against rustfs) and the test-only memory implementation.
- **Use the rig before prod** (`docker compose up --build -d` → authenticated gitcask on :8080,
  or simply `scripts/smoke.sh . 8090` against an existing rustfs).
- No new auth paths (§1.3). No LIST on hot paths. No unbounded buffering of packs in memory. No silent long
  operations (make it a task, narrate it).
- Every immutable response: `immutable` + strong ETag + Range; every ref-dependent response: SWR + ETag.
- Before changing the wire/store formats: proto is append-only; manifests/log entries must stay replayable by
  old readers within the retention window.
- Config: `gitcask.example.toml` documents every key; change it with the code.
- Test tiers: `just test` (fast, < 1 min), `just e2e`, `just warnings` (no unused/dead-code rustc warnings; the deliberate `unsafe_code` warns are not part of that gate), and
  `scripts/clippy-count.sh` (the `[workspace.lints]` set is deliberately *warn*-level and the tree carries
  historical warnings; the gate is **no regression against the base branch**, and `#[allow]` is never added to
  pass it); `scripts/smoke.sh` against rustfs before merging anything that touches
  smart HTTP, publish, sync or auth; the **simulation
  suite** `cargo test -p gitcask-server --test sim` (fault links per instance over one truth store: crash,
  partition, stale, lost response, orphan scenarios + randomized seeds `GITCASK_SIM_SEEDS`/`GITCASK_SIM_SEED`);
  `just test-slow` (ignored benches); `tests/e2e.sh` against a running server (`GITCASK_E2E_BASE_URL`). Never
  `cargo test --workspace --no-fail-fast` in a session; wrap ad-hoc cargo in `timeout`.
