# LFS — objects in the store

Context: **spec** for Git LFS on gitcask. For anyone touching `crates/gitcask-server/src/lfs.rs`, importing a
repository with LFS objects, or debugging "(missing)" in a push's
LFS pre-push. `AGENTS.md §1.4` lists LFS as part of the surface; this is the detail.

## 1. Protocol and storage (✅)
- Batch API `POST /{o}/{r}.git/info/lfs/objects/batch` (`operation = upload | download`, transfer `basic`),
  basic transfer `GET|HEAD|PUT /{o}/{r}.git/info/lfs/objects/<oid>`, `POST …/info/lfs/verify`. The front proxy
  authenticates these routes like the rest of gitcask and forwards the principal and grants.
- Objects live in the repository's prefix at `lfs/objects/<aa>/<bb>/<oid>` (`gitcask_proto::keys::lfs_key`) —
  sha256-addressed, immutable, served by `static_object` with the full static contract (strong ETag, 304,
  Range/If-Range, HEAD; `X-Accel-Redirect` to an edge's cache when one announces it, D23). `PUT` verifies size +
  sha256 before the store write. `lfs.max_object_bytes` (16 GiB) bounds an upload.

## 2. Not done / open
- `lfs.serve_via = "signed_url"` hands out presigned S3 URLs; the default `proxy`
  streams through gitcask or the edge.
- Size accounting of LFS bytes per repository in the overview.
