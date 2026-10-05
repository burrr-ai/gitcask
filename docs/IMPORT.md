# Full-history pristine import

The platform accepts `import_from.source` as `<project-id>/<repository-id>` for
internal Gitcask sources or a public HTTPS Git URL. Gitcask exposes the engine
protocol below; Cloud owns user authorization, durable intent, leases, retries,
202 responses and job status. The existing [tree snapshot initialize](INITIALIZE.md)
is a separate operation and continues to create a new parentless commit.

## Pin, persist, import

Create the target first using the ordinary repository PUT, with the matching
object format. POST `/{target-owner}/{target-repo}/api/import/resolve`:

```json
{"source":"templates/starter"}
```

The 200 no-store response is a snapshot. Persist this entire value before bulk
work. A resolve response lost before persistence may be resolved again; resolve
creates no durable engine job or reservation and commits no repository state.

```json
{"source":"templates/starter","object_format":"sha1","refs":[{"name":"refs/heads/develop","oid":"<full lower-case oid>","peeled":""}],"head":{"symbolic_target":"refs/heads/develop","oid":"<full lower-case oid>"},"snapshot_hash":"<sha256>"}
```

Refs are unique and name-sorted, selecting all heads/tags, including annotated
tag object IDs and recorded peeled targets. A missing peel hint is empty; bulk
validation derives it from the pinned tag object and always publishes the actual
peel for cold refs-first readers. A nonempty hint must match that object. A symbolic HEAD must name a selected branch;
a detached HEAD has an empty `symbolic_target` and its own commit OID. Hidden
provider refs, notes, pull refs, reflogs and unreachable objects are excluded.
Unborn/empty and shallow sources are rejected (422); complete independent staging
connectivity is required before any destination cache can participate. Internal SHA-1 and SHA-256 are supported;
external v1 accepts SHA-1 only, and target format must match (400 otherwise).

POST `/{target-owner}/{target-repo}/api/import` with the persisted snapshot:

```json
{"operation_key":"creation-123","snapshot":{"source":"templates/starter","object_format":"sha1","refs":[{"name":"refs/heads/develop","oid":"<full lower-case oid>","peeled":""}],"head":{"symbolic_target":"refs/heads/develop","oid":"<full lower-case oid>"},"snapshot_hash":"<sha256>"}}
```

`operation_key` is 1–128 printable ASCII characters without whitespace. First
success is 201; exact authorized replay is 200, no-store:

```json
{"operation_key":"creation-123","request_hash":"<sha256>","snapshot_hash":"<sha256>","seq":1,"refs_count":1,"head":{"symbolic_target":"refs/heads/develop","oid":"<original oid>"},"replayed":false}
```

Every reachable object is streamed through isolated Git packing/indexing with
fsck and destination connectivity checking, independently stored under target
`wal/`, then all refs, HEAD and a bounded receipt commit in the existing manifest
CAS. Source code is never checked out or executed; the source is never pushed,
repacked or changed. Source deletion and loss of every cache do not affect the
target. Source ref movement after resolve cannot repin the import; unavailable
pinned objects positively missing from a complete local source/staging probe produce
409, never a silently newer import. Unknown external fetch/subprocess failure is
retryable 503; no human stderr text is used for classification. External servers may
refuse fetching no-longer-advertised OIDs; that also fails as unavailable.

Pristine means no committed WAL work, packs, checkpoint or initialize/import
receipt. The publisher checks this for every CAS generation and earlier accepted
request in its batch, including writes to other branches. Empty refs after later
ref deletion do not restore pristine state. Failed attempts may leave immutable
uncommitted uploads, but never visible refs. Different operation keys or payloads
conflict (409). Exact replay returns the original receipt without opening source,
following later source refs, or changing later target writes. Receipts survive
checkpoint/log retention; deleting and recreating the target starts a new lifetime.
All serving readers and manifest writers must be upgraded together so old protobuf writers cannot
silently discard newly added receipt fields.

## Reconcile and authorize

Every resolve/bulk attempt uses the existing resolved principal: target write and
internal source read permissions, including replay. Scopes missing return 404;
trusted forwarding preserves its listener-wide grants and existing 403 write
denial. Forwarded read requires only a verified nonempty principal, and write uses
`X-Gitcask-Write: 1`; there is no `X-Gitcask-Read` header. A forwarding platform
must authorize both repositories itself. External URLs require no credentials and
v1 does not support private OAuth, Basic credentials or token-bearing URLs.

After lost success or source grant revocation, target read alone can confirm the
committed result with `GET /{o}/{r}/api/import/receipt?operation_key=...&request_hash=...`.
Receipt/import replay revalidates the target even when ordinary reads opt into
`wal.freshness_ttl`. Matching receipt returns 200 and `replayed:true`; absent receipt is 404 and key/hash
mismatch is 409. It never touches source. This endpoint reports a Git commit,
not pending engine job state. Cloud can reconcile before reacquiring source grants.

JSON request/response contracts use `Content-Type: application/json` and `Accept:
application/json`. Both resolve and import support the existing `Accept:
text/event-stream` task/progress/result/error envelope (HTTP 200; terminal error
contains its operation status). Tasks are per-instance, replayable and ephemeral;
object work runs on the bounded bulk runtime and continues after HTTP disconnect.
Phase-2 drain refuses new resolve/import work with 503. Acquisition has a deadline;
deadline expiry kills and verifies every owned Git process group before unwinding
the acquisition future and removing scratch. Process verification gets a two-second
poll bound and three-second foreground grace. If death cannot be verified, a cleanup
supervisor retains the future, ingest lock and scratch and retries verification;
a retryable error never claims that deferred resources are already gone. The
container includes procps for kill/ps. Index-pack and connectivity use supervised
async native children only on the import path; receive-pack ingestion is unchanged. The WAL publisher handles uncertain CAS outcomes independently of
acquisition cancellation. No engine identity, Redis, durable job DB, node routing
or Cloud store credentials are introduced.

## Preservation boundaries and bounds

Gitlinks and `.gitmodules` remain unchanged, with no recursive submodule fetch.
**LFS payload transfer is unsupported:** recognized LFS pointer headers in any
acquired historical small blob (not just the tip) cause 422 before publication,
even when source payload exists. Detection is conservative and includes pointer-like
symlink blobs; ordinary symlinks are preserved. No successful target is advertised
as having imported absent LFS payloads.

External URLs must be credential-free HTTPS on port 443, without query/fragment.
DNS results are resolved once per acquisition, all addresses must be public, and
the HTTPS client pins exactly those addresses while retaining TLS hostname/cert
validation. Loopback, private, link-local, CGNAT, metadata, documentation,
multicast/reserved IPv4, the Azure platform virtual IP (`168.63.129.16`), IPv4-mapped IPv6 and IPv6 transition/special ranges are
rejected. No environment proxy is used. All redirects are refused, including
same-origin redirects: callers must provide the canonical smart-HTTP Git URL.
Dumb HTTP fallback is unsupported. Git sees only a scoped loopback relay exposing
GET info/refs and POST upload-pack; arbitrary paths, receive-pack and headers are
not forwarded. Authorization and cookies cannot reach the source.

Git runs with a cleared environment, no system/global config, credential helpers,
replace objects, auto-maintenance, hooks or non-HTTP protocols. Only isolated
scratch/staging is written; destination never gains durable alternates. Packs
stream to files/index-pack; no complete pack is buffered in memory.

`[import]` defaults: 1024 refs, 1,000,000 acquired objects, 1 GiB aggregate external
response bytes and output pack (also bounded by `server.max_push_bytes`), 15-minute
acquisition timeout, 30-second resolve timeout. Resolve advertisements are capped
at 4 MiB transferred and 1 MiB parsed Git output. JSON snapshot/body is capped at
1 MiB; resolve input at 4 KiB. Names are capped at 1024 bytes. The object limit is
checked on independently acquired history before publishing; compressed transfer
may require indexing first. The source's ordinary internal materialization uses
existing pack/store limits. These are resource admission limits, not per-tenant
quotas. Cloud should allow the acquisition deadline plus publisher/store recovery
when setting its long HTTP deadline, and reconcile uncertain results by receipt.

| Status | Meaning |
|---|---|
| 400 | Invalid snapshot/hash/format/source URL or unsafe DNS result |
| 401 / 403 / 404 | Existing auth/permission/missing-repository contract |
| 409 | Nonpristine, operation conflict, or pinned source unavailable |
| 413 | JSON, refs, transfer, output pack or object bound exceeded |
| 422 | Empty/unborn source, LFS pointer history, or source requiring auth/redirect/dumb HTTP |
| 503 | Resolve busy, deadline, source DNS/transport, store/auth failure or drain; retry fixed request |

Non-503 errors use the existing plain-text envelope; 503 uses retryable JSON with
Retry-After (import failures identify `import_unavailable`). Unknown Git acquisition/audit or source transport errors return retryable503;
Cloud retries the persisted snapshot and never repins it. External servers
refusing a no-longer-advertised OID may also return this conservative retryable
class when the transport cannot prove the precise source absence.

## Exact canonical hashes

Append fields as decimal UTF-8 byte length, ASCII `:`, exact UTF-8 bytes, with no
separator. Compute SHA-256 and encode lowercase hex. This avoids JSON key ordering,
Unicode/HTML escaping, or Go/Rust serialization differences.

- Snapshot prefix is `gitcask-import-snapshot-v1\n` (literal LF). Fields: source,
  object_format, head.symbolic_target, head.oid, decimal refs count (a string),
  then each sorted ref's name, oid, peeled. The snapshot_hash field is excluded.
- Request prefix is `gitcask-import-request-v1\n` (literal LF). Fields: target
  owner, target repo (validated URL segments), operation_key, snapshot_hash.
  Thus the same snapshot/key for a different target has a different fingerprint.

Persist source exactly as returned by resolve; OIDs are lowercase/nonzero and
peeled is always present, including empty strings. Hash fixtures and tests live
in the import modules and Cloud counterpart. [ROUNDTRIPS.md](ROUNDTRIPS.md) owns
bucket budgets. Security controls use [Git configuration](https://git-scm.com/docs/git-config)
[Azure platform virtual IP](https://learn.microsoft.com/en-us/azure/virtual-network/what-is-ip-address-168-63-129-16),
and [reqwest client DNS/redirect/proxy settings](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html).

## Reproducible validation

`RUSTUP_TOOLCHAIN=1.97.1 timeout 300 cargo test -p gitcask-server --test import`
runs synthetic HTTP/Git preservation, cross-instance replay/race, crash/fault,
authentication, bounds and store budgets. `cargo test -p gitcask-server --lib
tls_tests` exercises real TLS with a synthetic CA, Git acquisition and redirect/
credential stripping through an injected test-only client; production has no
loopback or TLS bypass. `cargo test -p gitcask-server --test sim` exercises the
shared WAL crash/partition/replay protocol. `scripts/import-smoke.sh BASE` is the
standalone synthetic HTTP contract smoke, usable after shell/process restarts.
