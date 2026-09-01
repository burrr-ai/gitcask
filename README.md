English | [한국어](README.ko.md)

# gitcask

gitcask is a stateless git server that uses S3-compatible object storage as its only persistent layer. Each repository is stored in the bucket as a write-ahead log; servers hold only caches. There is no database and no leader election, and operating cost is proportional to pushes rather than to the number of repositories, so idle repositories cost nothing to keep.

It is intended for platforms that create and delete repositories programmatically, one per user project.

- **git over HTTP** — clone, fetch, push and LFS work with standard git clients.
- **JSON API** — read trees, commits and diffs; commit files, create branches and merge, all without a clone or working directory.
- **Webhooks** — each ref change is delivered once, from a durable cursor, and can be replayed.
- **Stateless servers** — every instance can serve every repository. A new instance starts serving refs within a few seconds.
- **Built-in authentication** — the platform signs a JWT with its own key; gitcask verifies it with the public key. No user database is involved.

## Where it came from

Vicent Martí described this architecture in Cursor's [*Git at any scale*](https://cursor.com/blog/git-at-any-scale) — Cursor calls it Continuity and runs a large monorepo on it. Tobi Lütke reproduced it as [walgit](https://github.com/tobi/walgit), and gitcask is a fork of walgit. Cursor noted that the design "scales in both directions": one huge repository, or a vast number of small ones. gitcask is built for the latter: many small repositories, created and deleted by a platform. What changed relative to walgit, and why, is recorded in [docs/DIRECTION.md](docs/DIRECTION.md).

gitcask exists because Cursor published the design and Tobi published the code. Thank you both.

## Try it in five minutes

```sh
docker compose up --build --wait
TOKEN=$(docker compose run --rm token --config /etc/gitcask/gitcask.standalone.toml token mint \
  --key /run/secrets/gitcask-private.pem --principal local-demo --scope local/demo:admin --ttl 1h)
curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/local/demo
git clone "http://ignored:$TOKEN@127.0.0.1:8080/local/demo.git"
cd demo && git commit --allow-empty -m first && git push -u origin HEAD:main
```

This starts a local S3-compatible store (rustfs) and one gitcask process. The token is a demo JWT signed with a throwaway key. In production the platform signs tokens with its own Ed25519 key and gitcask holds only the public key. Git sends the token in the Basic-auth password field; the username is ignored.

Note on token lifetimes: a token a person pastes into git is saved by the OS credential helper and reused. If it has to live long — weeks rather than hours — narrow its scope accordingly, to one repository with minimal permission. Backend tokens minted per request can expire in minutes.

## How it works

A repository lives under `repos/<owner>/<repo>/` in the bucket as a write-ahead log. A push uploads an immutable pack and a log entry, then updates a small manifest with a compare-and-swap. The CAS is the only point of consensus; no election or quorum is involved. When two instances race, one succeeds and the other retries.

A read begins with a conditional GET of the manifest. In the common case the store answers 304 and the local copy is used; otherwise the server applies the new entries before responding. A push that has been acknowledged on one instance is therefore immediately visible on all of them.

If every server disappears, the data remains in the bucket. A new instance pointed at it serves refs within a few seconds and downloads packs on the first object request. Maintenance work — checkpoints, compaction, integrity audits, garbage collection — is driven by markers written on push, so repositories without recent pushes are not visited at all.

The full design, including the reasoning behind each decision and the round-trip cost model, is documented in [AGENTS.md](AGENTS.md).

## Scope

Some things are intentionally left to the calling platform:

- **Repository listing and search.** Listing would require bucket scans, which this design avoids; the platform's database is expected to hold the authoritative list.
- **Users and login.** gitcask receives an opaque principal string and verifies a signature. Identity, permissions and teams remain in the platform, so there is no account data to synchronise.
- **CI, issues, UI.** The webhook can drive any CI system and the API can back any UI. The boundary and its rationale are described in [docs/PRODUCT.md](docs/PRODUCT.md).

Data can always be taken out: `git clone --mirror` exports a repository (with `git lfs fetch --all` for LFS content), and a consistent copy of the bucket — taken while writes are paused, or via S3 versioning or point-in-time replication — is a complete backup: a fresh deployment pointed at it serves as-is.

## Running it

```sh
# build (needs the Rust version in rust-toolchain.toml, plus protoc)
cargo build --release -p gitcask-cli
# or: docker build -t gitcask -f Containerfile .

# one machine: gitcask with authentication on :8080, backed by local rustfs
docker compose up --build -d
curl http://127.0.0.1:8080/healthz
```

- [`gitcask.standalone.toml`](gitcask.standalone.toml) — the single-process configuration. A good starting point.
- [`gitcask.example.toml`](gitcask.example.toml) — every configuration key, with defaults and comments.
- [`deploy/nginx.conf.example`](deploy/nginx.conf.example) — an optional nginx in front, for public TLS and byte offload.

Roles (`server.roles`): `serve` (git, API, LFS), `maintain` (checkpoints, compaction, fsck), `events` (the webhook bridge). An empty list enables all three. Any number of hosts can share one bucket.

## Developing

```sh
just test          # fast hermetic tier (< 1 min)
just e2e           # real git against a running server (~20 s)
just ci            # what a merge requires: warnings, clippy, test, e2e
scripts/smoke.sh . 8090                    # end-to-end against local rustfs
cargo test -p gitcask-server --test sim    # fault injection: crashes, partitions, stale reads
```

```
crates/
  gitcask-proto    protobuf schema (wal.proto), log framing, store keys
  gitcask-store    the ObjectStore trait; S3 backend, leases, retries
  gitcask-git      bare repositories on disk, receive-pack, pack ingest, refs
  gitcask-wal      sync levels, publish (group commit + CAS), checkpoints, tasks
  gitcask-server   axum: smart HTTP, LFS, auth, JSON API, maintainer, events bridge
  gitcask-config   gitcask.toml (+ GITCASK__ env overrides), fail-closed validation
  gitcask-cli      gitcask serve|import|migrate|compact|wal|repo|token
```

Further documentation: [docs/PRODUCT.md](docs/PRODUCT.md) (product boundaries), [docs/DIRECTION.md](docs/DIRECTION.md) (this fork's decisions), [docs/OPERATIONS.md](docs/OPERATIONS.md) (the runbook), [docs/MIGRATION.md](docs/MIGRATION.md) (moving from Gitea), [docs/ROUNDTRIPS.md](docs/ROUNDTRIPS.md) (the cost model), [docs/EVENTS.md](docs/EVENTS.md), [docs/INTEGRITY.md](docs/INTEGRITY.md), [docs/LFS.md](docs/LFS.md).

## Contributing

Development, tests and DCO requirements are in [CONTRIBUTING.md](CONTRIBUTING.md). Please report vulnerabilities privately as described in [SECURITY.md](SECURITY.md). Participation is governed by the [Contributor Covenant](CODE_OF_CONDUCT.md).
