# gitcask — direction and decisions

This document fixes what gitcask is, where it differs from its origin (walgit), and which decisions are
settled. How far the *product* goes (core vs platform, OSS vs cloud) is decided by `docs/PRODUCT.md`.
Read this before touching code — human or agent. `AGENTS.md` has been rewritten for this fork and the two
documents agree; if a conflict ever appears, **this document wins** and the other one gets fixed.
gitcask was renamed from repohub on 2026-09-01 before publication because the old name collided with an existing project and did not express its Bitcask lineage.

## 1. What gitcask is

**A stateless git storage and serving layer with an object store (S3) as the source of truth.** A repository
is a WAL in S3; a server is a cache that may be wiped at any time.

- git itself is not modified. `git upload-pack` / `index-pack` / `repack` and the rest of the plumbing are
  called as-is; gitcask decides only *where the bytes live* (packs in S3 under `wal/`, the current pack set in
  `manifest.pb` behind a CAS).
- The server's local disk is a cache. Idle repositories are evicted (`cache.evict_idle_after`) and a container
  that restarts empty rebuilds everything from S3. No persistent volume.
- Provenance: Cursor's *Git at any scale* (`docs/reference/cursor-git-at-any-scale.md`), reproduced by
  Tobi Lütke as walgit (2026-08-23), taken and renamed. History and the license file were not imported.

## 2. The workload — the opposite of walgit's

| | walgit (origin) | gitcask (us) |
|---|---|---|
| Goal | one company's 57 GiB monorepo served from a machine smaller than it | SaaS: thousands–tens of thousands of unrelated users × ~20 **small** repositories each |
| Repo size | tens of GB; one pack larger than the disk | a few MB – a few hundred MB; packs always fit on disk |
| Repo count | dozens, placed by hand in a config file | created automatically on sign-up; unbounded |
| Concurrent push | hundreds of developers into one repository | one user per repository, plus that user's agent |
| Auth | company IdP login, global write/admin flags | comwit issues scoped JWTs; gitcask verifies with a public key |
| Metadata | none (replaced by S3 enumeration) | **comwit's RDB** (repository list, owner, last_push_at) |

Because of this inversion, everything walgit built for *one big repository* is removed, and everything that
scales with *repository count* is reworked.

## 3. Settled decisions

1. **Identity and the permission model live outside gitcask.** comwit judges sessions/permissions and issues
   EdDSA JWTs; gitcask verifies only the signature and the repository scopes with a public key/JWKS. There is
   no login, no user DB, no sessions, no token issuance, no OIDC, no TLS.
2. **The repository list and metadata live in comwit's RDB.** The enumerate-every-repository APIs
   (`registry.list()` + S3 HEAD) are gone; the maintainer takes its work from push-driven markers, never from
   an S3 scan.
3. **comwit creates repositories.** `auto_create_on_push` is off. comwit inserts the RDB row, then calls
   `PUT /{owner}/{repo}`.
4. **comwit reads user files via `git clone`** (option A). The repository-browsing JSON API
   (`/{o}/{r}/api/tree|blob|commits…`) is not used yet but is **kept for the next stage**.
5. **Rust stays; this is a fork, not a rewrite.** Upstream (tobi/walgit) is not tracked.
6. **The cache disk is node-local SSD (emptyDir).** No tmpfs, no FUSE mounts, no remote pack serving. Packs
   are always downloaded whole.
7. **LFS is on** (D5). Big files go through LFS; the object cap is 1 GiB.

## 4. Keep / remove, by feature

### Kept (the core)
- Smart HTTP push/fetch (`smart.rs`, `gitcask-git/receive.rs`, calling `git upload-pack`)
- WAL publish (CAS + group commit), sync, checkpoints, compaction, evict (`gitcask-wal`)
- The S3 backend (`gitcask-store`)
- The maintainer loop (`maintain.rs`) — reworked to be marker-driven
- rev-index / fsck, drain
- EdDSA JWT verification + repository scopes, plus `forwarded` mode for existing proxies
- Repository create/delete (`admin.rs`: `PUT/DELETE /{o}/{r}`)
- Operational status API + SSE (`/{o}/{r}/api/overview|ops|tasks`, `sse.rs`)
- The repository browsing API (`web/api/`)
- The webhook bridge (`events.rs`, `bridge.rs`, `[events]`), `/_events/notify`
- health / metrics / telemetry
- LFS + `static_object.rs` (on, D5)
- CLI: `serve`, `repo *`, `wal *`, `compact`, `import`, `migrate`, `synth`, `token`

### Removed
| Group | What | Why |
|---|---|---|
| Auth | OIDC/browser login (`web/login.rs`), token **issuance endpoints**, install scripts (`setup.rs`, `/services/public/*`, `setup.json`), built-in TLS (`tls.rs`) | identity and issuance belong to the platform; gitcask only verifies a public key |
| Bundles | the `gitcask-bundle` crate, `bundles.rs`, CLI `bundle`, `[bundles]`, bundle-uri advertisement | small repositories clone instantly |
| Big-repo machinery | in-process upload-pack (`upload_gix.rs`), the remote reader (`remote.rs`), `store_mount`, prewarm, tier-2 base/history packs, `cache.mode = budget` | packs always fit locally |
| Placement & forwarding | `[placement]`, the push broker (`forward.rs`, `push_broker_*`), upstream follow/mirror (`follow.rs`, `mirror.rs`, `[upstream]`) | proxy routing replaces placement; nothing follows external repositories |
| Hosting policy | push policy (`policy.rs`, `docs/POLICY.md`), per-repo settings (`settings.rs`), the all-owners listing API, `/services/api/instance`, CLI `config|policy|settings` | one user per repository; one global config suffices |

The removals were carried out as the scoped tasks listed in §5.

## 5. Progress

| Task | Status |
|---|---|
| 01 auth → forwarded principal | ✅ merged |
| 02 remove bundles | ✅ merged |
| 03 remove big-repo paths (remote reader, upload_gix, store_mount, prewarm, tier-2) | ✅ merged |
| 04 remove placement / push broker / upstream follow | ✅ merged |
| 05 remove policy / settings / listing APIs | ✅ merged |
| 06 pending-marker maintainer (D1, AGENTS D40) | ✅ merged |
| 07 S3 transient-error retry (AGENTS D42) | ✅ merged |
| 08 1,000-repository load spike script (`scripts/spike.sh`) | ✅ merged — 118 store requests over 5 idle minutes; identical at n=100/1000 |
| 09 cache-eviction loop (never called since the origin) | ✅ merged — cache empty after 30 s idle, cold clone p50 134 ms |
| 10 transient store errors → 503 + Retry-After on every route | ✅ merged — 503 while rustfs is down, 201 after recovery |
| 11 parallel maintainer workers (default 8), oldest markers first | ✅ merged — 1→8→32 workers: 22 / 111 / 154 repos/s warm. 50k spike v2: all 7 stages OK, idle 120 vs 118 |
| 12 maintainer heartbeat cleanup | ✅ merged |
| 13 fsck only after compaction / periodically (no full fsck per new repository) | ✅ merged — cold drain still 13/s, so not the bottleneck |
| 14 cold-open concurrency (blocking fs/git waits pinned async threads) | ✅ merged — 2 threads: 32 concurrent cold opens 1,077→195 ms, cold drain 13→150 repos/s |
| 15 compare API (`…/api/compare/{base}...{head}`: merge-base semantics, commit list, file stats, 2 MiB patch cap) | ✅ merged |
| 16 correctness batch (committed pushes always answered; push 503s; discovery auth; defaults S3/auto-create off) | ✅ merged |
| 17 bucket GC (superseded packs, folded logs, old checkpoints; retention window + `materialize --at-seq` contract; converges in 2–4 s) | ✅ merged |
| 18 events backstop (pending ∪ cached) + global 10k task-record cap + evict on spawn_blocking | ✅ merged — known limit: if the marker PUT and the bucket notification are lost together, the sweep misses that push (webhook delayed only; data unaffected) |
| 19 OpenAPI generation (utoipa) + Scalar docs — `/api/v1/openapi.json`, `/api/v1/docs`, both behind auth | ✅ merged |
| 20 routing unification (hand-rolled dispatch removed; all axum) | ✅ merged |
| 21 gitcask-git 2,958 lines → 11 modules (public paths unchanged) | ✅ merged |
| 22 web/api.rs split (mod/view/git/handlers) + error unification (six `auth_err` copies and pktline duplication gone; git failures classed 400/404/409/500) | ✅ merged |
| 23 publish commit-path extraction (389→149 lines) + compaction/checkpoint leases into RepoHandle | ✅ merged |
| 24 remove `import --direct` (1,304 lines) — the second manifest-CAS implementation dies; import regression test added | ✅ merged |
| 25 remove the GCS backend, dead config and unused deps; fault/memory behind the `testing` feature (−3,212 lines) | ✅ merged |
| 26 stateless auth gate (mint/static, repo scopes, Basic/Bearer, streaming proxy) | ✅ merged, **superseded by 34** |
| 27 bulk Gitea migration (resumable, LFS, `docs/MIGRATION.md`) | ✅ merged |
| 28 write API, part 1 — branch/tag CRUD, annotated tags, archive | ✅ merged |
| 29 shared-cache retention — GC expires `cache/api` and `cache/archive` (zero extra LISTs) | ✅ merged |
| 31 sim flakiness root-caused — shared temp-file race in local state persistence fixed; sim 20/20 | ✅ merged |
| 32 write API, part 2 — batch file commits; policy-free merge/squash/ff-only | ✅ merged |
| 33 publication readiness — five-minute path, SECURITY/CONTRIBUTING/CoC, CI gate fixed | ✅ merged |
| 34 verify JWTs inside gitcask, drop the gate | ✅ merged — EdDSA public key/JWKS, scopes, Basic/Bearer, offline token CLI, one process |
| 35 operations runbook (`docs/OPERATIONS.md`) — verified metrics table, symptom-first diagnosis, recovery | ✅ merged |
| local smoke (`scripts/smoke.sh`, rustfs) | ✅ 51/51 — re-run on every merge |
| `AGENTS.md` / `GOAL.md` / `README.md` rewrites | ✅ |

Remaining (outside gitcask, or later):
- comwit integration: call `PUT/DELETE /{owner}/{repo}` on project create/delete, issue user git tokens in the
  same EdDSA claim format, consume the `[events]` webhook (D6)
- (D10) putting the browsing API to use

## 6. Vocabulary

- **WAL**: `manifest.pb` + `wal/<sum>.pack` + `log/<seq>.pb` under `repos/<owner>/<repo>/`. One push = one seq.
- **manifest CAS**: swapping `manifest.pb` with S3 `If-Match`. The only consensus point. ~1 write/s ceiling.
- **group commit**: folding same-repository pushes arriving within `wal.batch_window` into one CAS.
- **materialize**: building a local bare repo from S3 packs and refs. `wal materialize --at-seq N` restores a
  past point in time.
- **compaction**: `git repack`-ing several push packs (tier 0) into one (tier 1), uploading it, and removing
  the old ones from the manifest — the LSM SSTable merge, for git.
- **evict**: deleting an idle repository's local cache directory. Not data loss.

## 7. Operating decisions

These are comwit's actual operating choices. In particular D2 (size limits), D7 (new projects only),
D8 (AWS S3) and D11 (2 vCPU) are values comwit picked for itself, not defaults gitcask imposes on any other
deployment.

| # | Decision | Content |
|---|---|---|
| D1 | maintainer candidate selection | S3 `pending/<owner>/<repo>` markers: written on push success, consumed by `LIST pending/`, deleted when done. No RDB involvement |
| D2 | size limits | 100 MB per file, 2 GB per push (GitHub's numbers). No whole-repo cap; monitoring only |
| D3 | push rate limiting | at the proxy. gitcask does none |
| D4 | repository deletion | hard delete (`DELETE /{o}/{r}` deletes from S3 immediately). Any recovery policy is comwit's |
| D5 | LFS | on. `lfs.max_object_bytes = 1GiB` |
| D6 | user git auth | comwit issues EdDSA scoped JWTs; HTTPS Basic (username ignored + token). gitcask verifies directly with the public key/JWKS |
| D7 | existing data | new projects go to gitcask; existing ones convert on access |
| D8 | storage | AWS S3 |
| D9 | cache eviction | `cache.evict_idle_after = "2h"` |
| D10 | using the browsing API (option B) | after 06 (the maintainer rework) |
| D11 | instance size | **2 vCPU**, fixed for years. No optimisation raises thread counts; throughput comes from more instances. Runtime defaults (bulk threads etc.) target 2. Benches and spikes stay small (300–1,000 repositories) — they exist to show trends |
| D12 | storage backends | **S3 only.** The GCS backend was removed 2026-08-31 (1.7k unused lines + a separate dependency tree). If ever needed, recover it from git history |
| D13 | product boundary and the gate | (2026-09-01, **superseded by D14**) authentication lived in a separate gate process in the OSS core. Retired by task 34 to remove the trusted headers, the duplicated path-to-permission table and the second process |
| D14 | product boundary and JWT | (2026-09-01) the OSS core = auth · git transport · read/write API. gitcask verifies EdDSA JWT signatures and repository scopes itself but **owns no identity**; issuance belongs to the platform or the offline CLI. The cloud = multi-tenancy · billing · operations. CI, issues, PRs, UI and repository listing are out of scope |

Both the comwit backend and users' git CLIs send JWTs to the same single gitcask process. Only deployments
that already have their own IdP proxy choose `server.auth_mode = "forwarded"`.
