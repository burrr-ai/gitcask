# Operations runbook

Context: for whoever operates gitcask instances and their S3 bucket — incident diagnosis, capacity planning
and recovery procedure. The design starts from [GOAL](../GOAL.md); the WAL and maintenance principles are in
[AGENTS](../AGENTS.md); the cost of every bucket round trip is in [ROUNDTRIPS](ROUNDTRIPS.md). This document
does not repeat them; it is about where to look after an alert fires.

## 1. First look

`/healthz` only says the process answers; `/readyz` only says whether serving drain has started. Neither
touches S3, so a 200 does not mean the bucket is fine.

```sh
curl -si "$BASE/healthz"
curl -si "$BASE/readyz"
curl -s "$BASE/metrics"

gitcask --config gitcask.toml wal pending
gitcask --config gitcask.toml repo info owner/repo
curl -s "$BASE/owner/repo/api/overview"
curl -s "$BASE/owner/repo/api/tasks"
```

- `wal pending` is a manual check that LISTs `pending/`. Do not call it repeatedly or put it on a request path.
- `repo info` runs `sync_full()` and pulls packs local, so it is expensive on a cold repository. Use it only
  when digging into one repository; for refs and maintenance state, look at `api/overview` first.
- `api/tasks` records are per instance. Keep the response's `hostname` together with the log `request_id`.
- A metric appears only after its code path has run once. A missing series may mean zero — or that this role
  or unit has simply not executed in this process yet.

With `telemetry.log_format = "json"`, every span-close line carries `span.name`, `elapsed_ms`, `outcome`,
`repo` and `request_id`. The span names used in the procedures below are the real names in the code.

## 2. Key metrics and alerts

The thresholds below are starting points. Tune the durations to your traffic baseline and SLOs, but keep the
comparisons against configuration values as they are. Sum counters across all instances of a role; read gauges
labelled `host` per instance. Gauges with a `repo` label exist to narrow down a problem repository — they are
not a substitute for a repository listing.

| Name | Normal | When it moves | Suggested alert starting point |
|---|---|---|---|
| `gitcask_push_refused_total{reason}` | flat outside deploys | `connectivity` = object-closure failure on a new tip, `unpack` = pack parse/index failure, `draining` = a push during serving drain | one `connectivity`/`unpack` immediately; one `draining` outside a deploy window |
| `gitcask_publish_local_apply_failed_total` | 0 | the manifest CAS succeeded but applying refs locally on that instance failed. The next sync repairs it, but it is the lead on an immediate-visibility regression | on any increase |
| `gitcask_pending_marker_put_failures_total` | 0 | the push committed but the best-effort marker PUT that wakes the maintainer failed | on any increase; check that repository by hand |
| `gitcask_store_retries_total{op}` | usually flat | transient S3 errors (5xx, 429/throttling, connection failures) being retried internally | warn if still climbing after 5 minutes |
| `gitcask_store_requests_total{op,outcome}` | `ok`, ordinary `not_found`/`precondition_failed` | `retryable_error` = retries exhausted, or a transient error on a conditional write that is never retried at this layer; `error` = any other store failure | one `retryable_error`/`error` immediately; read the `store.*` log lines from the same moment for the exact S3 status |
| `gitcask_pending_markers` | 0 when idle; converges back to 0 in the passes after a push burst | maintainer backlog. A value equal to `maintenance.max_repos_per_pass` may mean at least one more page behind it | warn if > 0 for two whole maintenance intervals; page if pinned at the page limit |
| `gitcask_maintain_workers_busy{host}` | 0–`maintenance.workers`, 0 without backlog | pending work in progress. Pinned at the limit while backlog grows = not enough throughput | all workers busy for ≥ 2 intervals while `gitcask_pending_markers > 0` |
| `gitcask_maintain_units_total{host,kind,outcome}` / `gitcask_maintain_unit_seconds{kind}` | `outcome="ok"`, durations within your per-repo-size baseline | a checkpoint/compact/rev-index/fsck/gc unit failing or slowing down | any `outcome="failed"`; warn on sustained p99 above baseline |
| `gitcask_maintainer_heartbeat_timestamp{host}` | within two intervals of now; refreshed every 2 minutes even during long units | maintainer loop stopped, process paused, or store writes failing | `time() - value > max(2 * maintenance.interval, 5m)` |
| `gitcask_checkpoint_lag_entries{repo}` / `gitcask_checkpoint_age_seconds{repo}` | inside the active `wal.*` triggers at the moment a marked repo was planned | the checkpoint unit not keeping up with its triggers | non-zero beyond the entries/interval trigger for two passes. Age is the value at gauge-update time, not a running clock |
| `gitcask_checkpoints_total{outcome}` / `gitcask_checkpoint_seconds` | `outcome="ok"`, duration within your refs/tail baseline | checkpoint writes failing or suddenly slow | any `outcome="error"`; warn on sustained p99 above baseline |
| `events_bridge_lag_entries{repo}` | 0 after catch-up | `head_seq - cursor`; the webhook or the bridge is behind | > 0 for longer than `events.sweep_interval` |
| `events_bridge_gap_total{repo}` / `events_bridge_sweep_found_total` | flat | gap = the cursor fell behind WAL retention; sweep-found = the regular notification was missed and the backstop found it instead | one gap immediately; one sweep-found as a warning |
| `events_published_total{sink}` | grows when refs change | if lag is 0 and this grew, investigate the consumer side beyond gitcask | use together with lag/gap, not as a standalone alert |
| `gitcask_cache_disk_used_fraction` | below `cache.disk_high_watermark` | usage of the **whole filesystem** holding `cache.dir` is high | above the watermark for ≥ 2 × `cache.evict_interval` |
| `gitcask_cache_evicted_total` / `gitcask_cache_repos` | moves with the idle policy | pressure without eviction growth = every candidate is in use or deletion is failing; a spike = cache churn | sustained disk pressure with eviction stalled, or eviction abruptly above normal |
| `gitcask_lock_wait_seconds{lock}` | mostly instant; recorded waits below `telemetry.lock_wait_warn` | contention on `rw.read`, `sync_mutex`, `pack_mutex` | p99 above `telemetry.lock_wait_warn` for 10 minutes |
| `gitcask_runtime_stall_total` / `gitcask_runtime_stall_seconds` | flat | the tokio ticker ran > 2.5 s late | check logs on any increase. If the same line shows `inflight > 0`, treat as a real worker block/starvation — urgent |
| `gitcask_http_inflight` / `gitcask_tasks_running` | returns to the instance baseline | streaming requests that never finish, or long tasks | warn when baseline and client latency climb together |
| `gitcask_repo_missing_objects{repo}` | 0 | the last connectivity audit found an advertised ref pointing at a missing object | > 0 is a data-integrity incident, immediately |
| `gitcask_gc_deleted_total{kind}` | grows only when GC has something to collect | deletion trend. Zero here is not by itself a failure | never alert alone; judge together with failed due-GC tasks and bucket growth |

There is no dedicated store-latency Prometheus metric. Do not build dashboards on series that do not exist —
derive p50/p95/p99 from the JSON logs: `span.name = store.get|store.head|store.put|store.delete` with
`elapsed_ms` and `outcome`. Push breaks down into `receive.ingest`, `git.ingest_pack`,
`receive.connectivity`, `receive.publish` and `wal.publish` spans. For clone/fetch, total streaming time is
judged by the git client's own timing plus the band-2 progress lines; cold sync splits into `wal.sync`,
`wal.materialize`, `wal.reconcile_packs`, `wal.download_pack`, and pack computation into `git.upload_pack`.

`gitcask_push_refused_total` is not the sum of all push failures — only the three refusals in the table. A ref
conflict is reported as git protocol `ng` and can be HTTP 200; publish/store failures are judged by 503s or by
pkt-line errors in an already-started stream plus the logs. There is also no general HTTP status /
request-duration Prometheus metric today.

A repository with no checkpoint yet has no age series either; read `health.deep` and the checkpoint suggestion
in `api/overview` together. With `cache.disk_high_watermark = 0`, both pressure eviction and the disk-used
gauge are disabled.

The source of truth for integrity is `fsck.pb`. `gitcask_repo_missing_objects` carries only the missing-object
count; other fsck findings are read from `health.deep` in `api/overview` and the report. Audit cadence and
meaning are defined in [INTEGRITY](INTEGRITY.md) only.

## 3. Diagnosis, starting from the symptom

### Clone or push is slow

**Check, in order**

1. Keep the git output and wall time for the affected repository. Use band 2's
   `local copy is missing packs` / `local copy ready (...s)` to split before/after sync.
2. Look at `gitcask_store_retries_total`, store error outcomes and `store.* elapsed_ms`. If every repository
   is slow at once, suspect S3 or the network first.
3. Long `wal.materialize`/`wal.download_pack` with only the first request slow = cold cache. Long
   `git.ingest_pack` inside `receive.ingest` = pack indexing/fsck; long `receive.connectivity` = the object
   graph walk; long `wal.publish` = the PUT/CAS phase.
4. Check `gitcask_lock_wait_seconds`, `gitcask_runtime_stall_total`, `gitcask_http_inflight`. On an
   `async runtime stalled` line, read `inflight`, `tasks_running`, `lock_wait_max_ms` and `rss_mb` together.
5. In `api/tasks`, follow the `materialize` task's progress and result. A second operation on the same
   `(repo, kind)` joins the existing task.

**Common causes and actions**

- Cold materialize on the first object request: normal. Confirm it matches pack size and S3 throughput; give
  the cache disk and instance lifetime room.
- Store retries/latency: go to the S3 outage procedure below.
- Repeated evict/materialize churn under disk pressure: grow the cache filesystem, or separate the cache from
  other files' filesystem.
- Lock waits or runtime stalls: find the blocking section in that `request_id`'s span tree and report it as a
  bug. Blocking git/fs work on the serving runtime is never a tuning matter.
- Clone still slow after `local copy ready` with store and locks healthy: isolate pack computation with the
  `git.upload_pack` span. CPU-saturated → add serving instances; a single repository larger than the disk →
  a bigger instance. Client time far above the span → investigate the transport path.

### A push gets 503

**Check, in order**

1. Immediately: `curl -si "$BASE/readyz"`.
2. `503` + `Retry-After: 15` + `{"status":"draining"...}` = serving drain, phase 2.
3. `/readyz` 200 but request logs show 503s and `gitcask_store_retries_total` or
   `gitcask_store_requests_total{outcome="retryable_error"}` is climbing = S3 outage. The plain JSON response
   is `{"error":"store_unavailable","retryable":true}`.
4. Cross-check `gitcask_push_refused_total{reason="draining"}` against deploy times. The store span's `error`
   carries the actual connection/status cause.

**Actions**

- Drain: retry against another ready instance. Never put a terminating instance back into ready.
- S3 outage: do not hammer writes into a retry storm; restore S3. Afterwards confirm with a refs read and one
  small push.
- A store failure after the git sideband stream has started cannot change the HTTP status and may end as a
  pkt-line error. Judge git client failures and server store logs together.

### A push succeeded but the ref is not visible on another instance

This is not eventual consistency to be tolerated — in the default configuration it is a correctness bug. But
first confirm both instances use the same bucket + prefix and `wal.freshness_ttl = "0s"`; a positive TTL skips
the manifest GET for that long.

**Reproduce and collect evidence**

```sh
new_oid=$(git rev-parse HEAD)
git push "$INSTANCE_A/owner/repo.git" HEAD:refs/heads/main
GIT_TRACE_CURL=1 git ls-remote "$INSTANCE_B/owner/repo.git" refs/heads/main \
  >ls-remote.out 2>ls-remote.trace

curl -s "$INSTANCE_A/owner/repo/api/overview" >overview-a.json
curl -s "$INSTANCE_B/owner/repo/api/overview" >overview-b.json
gitcask --config gitcask.toml repo info owner/repo
```

If B's first request returns the old OID, do not paper over it with a retry. Keep: `$new_oid`, the push time
and result, the `x-request-id` from the ls-remote trace, both overviews' `hostname` + manifest version + seq,
both instances' commit SHAs and configuration (bucket/prefix/freshness TTL), and the `wal.sync`/`store.get`
spans from the same moment. Take B out of serving but do not wipe its cache or logs. The minimal regression
check:

```sh
export RUSTUP_TOOLCHAIN=1.97.1
cargo test -p gitcask-server --test e2e two_instances_consistency -- --nocapture
cargo test -p gitcask-server --test sim
```

### The disk is full

**Check, in order**

```sh
df -h /path/to/cache.dir
du -sh /path/to/cache.dir
```

1. Compare `gitcask_cache_disk_used_fraction` with `cache.disk_high_watermark`. It is the usage of the whole
   filesystem, not the size of the cache directory.
2. Read `gitcask_cache_evicted_total`, `gitcask_cache_repos` and the `cache disk above high watermark`,
   `cache repositories evicted`, `cache directory removal failed` log lines.
3. In `api/tasks`, look for work that is using packs — materialize, compact, fsck. The evictor skips busy
   repositories whose `sync_mutex` or repo write lock it cannot take immediately.

**Actions**

- Send new object work to other instances so in-flight work can finish and eviction can proceed.
- Grow the filesystem or remove non-cache occupants. Never delete the cache directory by hand.
- Above the watermark, the evictor removes least-recently-touched repositories toward
  `disk_high_watermark - 0.10`. Repositories past `cache.evict_idle_after` are removed on the next
  `cache.evict_interval` even without pressure.
- The cache is disposable, but an instance without room for the packs the WAL points at will keep failing.
  Size the disk so the largest repository *and* the scratch/output packs of work in progress fit together.

### Webhooks are not arriving

The contract and cursor recovery follow [EVENTS](EVENTS.md).

1. Is `events_bridge_lag_entries{repo}` > 0? Then the bridge or the webhook endpoint is failing.
2. Lag 0 and `events_published_total{sink}` grew: gitcask got a 2xx and advanced the cursor — investigate the
   consumer's dedup/processing logs.
3. `events_bridge_sweep_found_total` growing: the backstop sweep found unpublished WAL — fix the bucket
   notification flow.
4. `events_bridge_gap_total` growing: WAL before the cursor was folded past retention. Do not advance the
   cursor by hand; backfill from the last consumed seq with `wal ls`/`wal show`.
5. Restore the webhook and wait for the next notification or sweep. The cursor never moves before a 2xx, so
   the same batch may be delivered again.

### A fresh instance is slow

Refs-only requests read the manifest plus checkpoint/tail and need no packs. The first clone/fetch/push that
needs objects materializes the whole live pack set into `cache.dir`, and can take as long as the repository
is large.

- One `wal.materialize` task, `wal.download_pack` spans and one band-2 `local copy is missing packs`, then
  fast subsequent requests: normal cold cache.
- Repeating every time: check `gitcask_cache_evicted_total`, the disk watermark, whether the cache directory
  is writable, and the instance's lifetime.
- With `wal.prefetch_packs = true`, pack reconciliation starts in the background after a refs-only sync — but
  there is no prewarm or remote-pack path. The first object request still requires the full pack set to fit
  locally.

## 4. Capacity and placement

The default operating size is 2 vCPU ([DIRECTION D11](DIRECTION.md)). Scale by adding instances of the same
role, not by raising worker counts on one instance. Distinguish the signals:

- **Add serving**: store, disk and locks healthy, but client latency and `gitcask_http_inflight` are high on
  several instances at once. Concurrent git work on one repository also shares the
  `server.max_concurrent_per_repo` semaphore.
- **Add maintainers**: `gitcask_pending_markers` does not converge and every maintainer's workers stay busy.
  First confirm one worker has the disk headroom to materialize. Do not hide a CPU bottleneck by raising
  thread counts past the D11 default.
- **Store scaling/investigation**: adding instances makes store retries and `store.* elapsed_ms` worse
  together. More serving here only multiplies S3 request volume.

Size the disk so the sum of the following stays below the watermark:

1. Live packs + idx/rev/bitmap/commit-graph of the repositories concurrently active on this instance.
2. Packs being materialized by `maintenance.workers` concurrent workers.
3. Scratch/output packs from receive-pack ingest and compaction — during compaction, inputs and outputs
   coexist.
4. Headroom up to `disk_high_watermark`, plus whatever else lives on the cache filesystem.

The floor: the full live pack set of your largest repository plus its working scratch must fit on one
instance; otherwise you need a bigger disk. `cache.bulk_threads` (default 2) is the async lane dedicated to
pack materialization; blocking git/fs work runs on a separate blocking pool.

Know what grows with repository count and what does not:

| Grows | Does not grow with total repository count |
|---|---|
| Actual git data + WAL objects in the bucket; pending work created by pushes | Repositories the maintainer visits: only those with a `pending/` marker |
| The local cache of repositories this instance actually touched | Periodic `repos/` LISTs while idle: there are none |
| S3 requests proportional to pushes, compactions, checkpoints, GC | Checkpoint/fsck visits to unpushed repositories: never evaluated without a marker |
| Maintainer heartbeats proportional to live instances | Holding every repository's packs locally: only the working set before eviction |

If gitcask's periodic cost rises merely because idle/empty repositories multiplied, that is a D40 regression.

## 5. Incidents and recovery

### S3 outage

The S3 backend retries retryable failures on GET, HEAD, LIST, DELETE, multipart stages and unconditional PUTs
up to `store.max_retries` with full-jitter backoff. Conditional Create/Update PUTs are never retried at the
store layer (replaying one makes success ambiguous); the manifest CAS protocol re-checks whether the commit
actually landed on its failure path and owns its own CAS retry.

If retries are exhausted, HTTP requests that have not started responding become 503 + `Retry-After: 15`. A
push never reports success before the bucket ACKs the commit. In order:

1. Scope the op/key/error from store retry/error rates and `store.*` spans.
2. Reduce new write load; restore the S3 endpoint's connectivity, service health, rate limits and bucket state.
3. Verify the read path with a real refs read, not `/healthz`.
4. Push a small test repository and immediately confirm the OID from a different instance.
5. If any push was answered success *during* the failure, treat it as a correctness incident — by design there
   should be none.

### All instances lost

Start new instances pointing at the same config and bucket. There is no persistent-disk recovery and no
node-to-node state transfer.

```sh
gitcask-server --config gitcask.toml
```

Check `/healthz` and `/readyz`, then `ls-remote` and clone a representative repository. The first refs request
replays checkpoint + WAL tail; the first object request downloads full packs, so both are slow. That cold cost
and the normal round trips are defined in [ROUNDTRIPS](ROUNDTRIPS.md). Do not restore `cache.dir` from backup.

### Bucket corruption or accidental deletion

gitcask has no bucket-backup engine of its own. Production S3 should run with versioning on, plus
cross-account/region replication or S3 Inventory. When an incident happens, stop new writes, then:

1. Pin down the affected prefix and the first moment of damage. Never hand-delete packs/logs/checkpoints that
   `manifest.pb` points at, and never hand-craft a new manifest.
2. Restore deleted/corrupted objects to their original keys from S3 object versions/replicas. Verify checksum
   and size on immutable packs/logs.
3. Clone into a fresh cache and run `git fsck --full --strict`; check the maintainer fsck report as well.
4. Do not return to serving while `gitcask_repo_missing_objects > 0`. The full verdict procedure is
   [INTEGRITY](INTEGRITY.md).

If a force push merely moved a ref, do not roll the bucket back. Find the seq in the WAL and resurrect the old
OID as a new ref update. `wal ls` shows seqs and ref-update counts; the actual `old_oid` comes from `wal show`.

```sh
gitcask --config gitcask.toml wal ls owner/repo
gitcask --config gitcask.toml wal show owner/repo 42
gitcask --config gitcask.toml wal materialize owner/repo --at-seq 41 --out /tmp/repo-restore
git -C /tmp/repo-restore fsck --full
git -C /tmp/repo-restore push --force "$REPO_URL" \
  <old_oid>:refs/heads/<branch>
```

If `materialize --at-seq` fails explicitly as beyond retention, the needed logs/packs are already collected;
without S3 versions/replicas that point in time cannot be restored.

### Deploys and rollback

After SIGTERM/SIGINT, drain happens in two phases:

1. **Maintenance drain**: no new unit starts and the running unit is interrupted at once. Serving is normal
   and `/readyz` stays 200. Wait until units are gone or the 30-second bound ends.
2. **Serving drain**: `/readyz` turns 503 + `Retry-After: 15` and new fetch/push/LFS object work is refused.
   In-flight requests get `server.drain_timeout`; the listener stays open 2 more seconds so the load balancer
   can observe the readiness flip.

With no maintenance unit running, phase 1 can be too short to observe. `/healthz` stays 200 while the process
lives, so deploy routing must use `/readyz`. Roll back by shipping the previous image together with the config
that image understands as one unit; pre-1.0 there are no config/route compatibility shims, but the bucket
WAL/proto is append-only, so logs within retention must stay replayable. After a rollback, verify with a push
on one instance and a clone from another.

## 6. Routine checks

**Daily, or at handover**

- No growth in store retries/final errors, runtime stalls, local apply failures, pending-marker PUT failures.
- `gitcask_pending_markers` converges to 0 after bursts; maintainer heartbeats are fresh.
- Event lag is 0 and gap/sweep-found are flat.
- The cache filesystem is below the watermark and eviction follows the expected idle policy.

**Weekly**

- Check `gitcask_repo_missing_objects` and `health.deep` in `api/overview` for representative and
  recently-changed repositories. Fsck visits only marked repositories — do not read it as a full audit of
  long-unpushed ones.
- Check `gitcask_maintain_units_total{kind="gc",outcome="failed"}` and task logs for due GC. GC with nothing
  to collect does not move `gitcask_gc_deleted_total`.
- Watch total bucket bytes/object count via the provider's metrics or S3 Inventory. Sustained growth not
  explained by pushes or retention changes → start with whether the GC task converges after
  compaction/checkpoints on specific repositories.
- Re-compare the largest repository size, cache working set and compaction peak against disk headroom.

**Periodic recovery drills**

- Clone a representative repository on a fresh instance with an empty cache; `git fsck --full --strict`.
- Restore one S3 object version into an isolated bucket/prefix and verify its checksum.
- Restore a within-retention seq with `wal materialize --at-seq` and compare refs against a current clone.
- On a SIGTERM deploy, observe phase-1 serving and the phase-2 `/readyz` 503.
