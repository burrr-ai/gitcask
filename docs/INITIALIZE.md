# Initialize a repository from a pinned tree

`POST /{owner}/{repo}/api/initialize` initializes an **existing, pristine** destination
from one explicitly pinned commit in another Gitcask repository. It creates one new
root commit, with no parent and none of the source commit history. Every required
Git object is uploaded into the destination's normal `wal/` pack storage before
the normal manifest CAS publishes it. Deleting the source or either instance's
cache does not affect the destination.

```json
{
  "source": {"owner": "templates", "repo": "starter", "commit_oid": "0123456789012345678901234567890123456789"},
  "branch": "main",
  "operation_key": "project-creation-7",
  "message": "Initial project",
  "committer": {"name": "Project Builder", "email": "builder@example.test", "when": "2026-09-28T00:00:00Z"}
}
```

`source.commit_oid` must be a full, nonzero commit object ID (40 hex digits for
SHA-1, 64 for SHA-256), not a branch, tag, expression, or abbreviated ID. Both
repositories must use the same object format. The source commit need not remain
a branch tip, but its objects must still be available in the source's live packs.
`branch` is below `refs/heads/`; initialization also sets symbolic `HEAD` to it.
An optional `author` has the same shape as `committer` and defaults to it. Identity
timestamps must include an explicit RFC 3339 offset. No server time is substituted
into the commit. The caller supplies the message and both identities; these are
commit content, not authorization identities.

`operation_key` is 1–128 printable ASCII characters without whitespace. The body
is limited to 64 KiB. Unknown request fields are rejected. The request fingerprint
uses parsed fields in fixed order, lower-case commit IDs, and the effective author;
JSON whitespace/key order and an omitted author equal to the committer do not
change it. Other field changes, including timestamp spelling, change the fingerprint.

First success returns **201**; an exact retry returns **200**:

```json
{"ref":"refs/heads/main","commit_oid":"<new root oid>","tree_oid":"<source tree oid>","seq":1,"replayed":false}
```

The replay returns the original ref, commit, tree and WAL sequence with
`replayed:true`. It does not assert that the ref still has that value and never
moves a ref. Persist and reuse the entire original request after a lost response.
The sequence can exceed 1 if a failed publisher left an orphan log slot.

## Authorization and errors

Each attempt, including a replay, requires destination write permission **and**
source read permission under the existing authentication modes. Both permissions
are checked before looking up the source. Missing repository scopes return 404;
forwarded principals retain their existing 403 write-denial semantics. A committed
replay does not look up or rematerialize the source, so deleting it is compatible
with retry as long as the caller still has the required scope.

| Status | Meaning |
|---|---|
| 400 | Invalid request/identity/branch/full commit ID, non-commit source object, or different object formats |
| 401 | Missing or invalid credentials |
| 403 | Write denied by the existing forwarded-identity contract |
| 404 | Missing or unauthorized repository, or unavailable pinned source commit |
| 409 | Destination is not pristine, or a committed initializer has a different operation key or fingerprint |
| 413 | Request body or configured `server.max_push_bytes` pack limit exceeded |
| 422 | Source tree contains a Git LFS pointer; this operation does not copy LFS payloads |
| 503 | Temporary object-store/authentication-service failure; retry the identical request |

Errors use the existing API envelope: non-503 errors are plain text; 503 uses
`{"error":"store_unavailable","retryable":true}` (or the existing authentication
service equivalent) and `Retry-After`. Clients accepting `text/event-stream` receive
the normal notice/task/result/error envelope while work runs; its HTTP status is
200 and the terminal error packet carries the operation status. Work is registered
as an `initialize` task, runs on the bounded bulk runtime, and survives client
disconnects. An already committed retry is a fast plain JSON response.

Use `Content-Type: application/json` and `Accept: application/json` for plain
responses; success includes `Content-Type: application/json` and `Cache-Control:
no-store`. Non-503 errors use `text/plain; charset=utf-8`. A store 503 carries
`Retry-After: 15`; an introspection-service 503 carries `Retry-After: 5`.

Trusted forwarding uses the existing listener-wide grants, not repository scopes:
`X-Gitcask-Principal` plus `X-Gitcask-Write: 1`, and the verified
`X-Gitcask-Forward-Secret` in `introspect_forwarded` mode. A platform using this
transport must authorize both source read and destination write before forwarding.
See [SECURITY.md](../SECURITY.md#mixed-direct-and-trusted-proxy-authentication) for
header stripping and scheme selection. JWT/introspected identities require both
repository scopes. No initialize-specific authentication path is introduced.

## Scope and durability

The tree is copied unchanged, including binary blobs, executable bits, symlinks,
gitlinks and `.gitmodules`. Submodule commits remain external references, as in
ordinary Git; the operation never initializes submodules or fetches their URLs.
It accepts only Gitcask repository identities, never arbitrary Git URLs, and makes
no working-tree checkout. Git LFS pointer blobs are rejected before publication,
even if their payload is present in the source; no clone is reported successfully
initialized with a missing destination LFS payload. Detection inspects bounded
small blobs, including the version URL accepted by older Git LFS clients. Empty
files and ordinary blobs remain supported.

Pristine means `head_seq == 0`, no live packs, no log segments, no checkpoint and
no initialization receipt. A repository with all refs subsequently deleted is
**not** pristine. Failed attempts may leave unreachable immutable uploads under
the destination prefix; these do not publish refs or prevent an identical retry.
The check runs in the existing publisher against its CAS generation, after each
earlier accepted request in a batch, and again after CAS contention. An initializer
that wins may be followed by normal writes; an earlier accepted normal write
prevents initialization, even when it writes a different branch.

One bounded receipt in `Manifest.initialization` records the operation key,
SHA-256 request fingerprint and original result. It becomes durable in the same
manifest CAS as the ordinary PUSH entry, pack references and branch/HEAD update.
Normal publishing, compaction and checkpoints preserve it. There is no secondary
commit point, snapshot resource, shared pack or source-lifetime dependency. The
receipt survives log retention and later writes; deleting/recreating the destination
starts a new repository lifetime and discards its receipt.

Before enabling this operation, upgrade **all manifest writers**, including
maintainers: older binaries do not preserve unknown protobuf fields when rewriting
a manifest and could drop the receipt. The format extension is append-only and
existing WAL/checkpoint records remain replayable.

The request budget is in [ROUNDTRIPS.md](ROUNDTRIPS.md). This is deterministic Git
plumbing under [PRODUCT.md Rule A](PRODUCT.md), not repository-template policy.
