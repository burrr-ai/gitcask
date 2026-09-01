# Object integrity — the invariant and the audit

Context: **spec** for the one data invariant the WAL cannot express: *every object reachable from an
advertised ref is in a live pack*. For anyone touching `gitcask import`, the maintainer's `fsck` unit
(`crates/gitcask-server/src/ops.rs`, `maintain.rs`), or debugging `connectivity:
missing object` on a push / `NotFound` on a partial-clone fetch. Born from a large-repository recovery test (the monorepo's 1,952
missing blobs).

## 1. The invariant and where it can break
The WAL guarantees *which* packs are live and *which* refs point where; it does not know whether the packs hold
the refs' closure. Pushes cannot break it (receive-pack checks connectivity before publishing — that check is
what surfaced the hole). What can:
- **Import**: a pack set built from one ref selection and a ref snapshot taken from another. The default importer
  avoids that split by packing all source refs before publishing the selected heads and tags through the normal WAL
  path. `--reuse-packs` requires a self-contained source pack set; the audit below is the backstop for an incomplete
  or corrupt source repository.
- A compaction that drops objects (reachability from a stale tip), a superseded pack GC'd too early,
  a corrupt or truncated object in the bucket. None seen; the audit below is the detector for all of them.

## 2. Audit: the `fsck` unit (✅ 2026-08-21)
Lowest-priority maintainer unit (`maintenance.fsck_interval`, default 7 d). Runs
`git fsck --connectivity-only --no-dangling` (`ops.rs` `fsck`, `connectivity=1`) and writes the verdict to
**`repos/<o>/<r>/fsck.pb`** (`FsckReport {seq, at, host, missing[≤100 k], missing_total, problems, elapsed_secs, audited_seq}`;
overwritten, not WAL). Missing objects are a finding and corrupt objects a failure. Gauge
`gitcask_repo_missing_objects{repo}` is set from the report on every pass. Due immediately after compaction;
otherwise, once a report exists, due after `fsck_interval` only when the WAL advanced beyond `audited_seq`.
A new repository that has not been compacted is not audited on its first maintainer visit because receive-pack
already checked its connectivity. An interval of 0 removes the age delay but still requires a prior report and a
new push. Manual: `POST
/{o}/{r}/api/ops/fsck` with `connectivity=1` (WAL page) writes the same report. the monorepo on a complete local copy: ~10 min.
