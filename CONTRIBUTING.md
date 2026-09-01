# Contributing to gitcask

gitcask's central constraint is simple: the bucket is the repository; local disk and memory are caches. Read
[`GOAL.md`](GOAL.md), [`docs/DIRECTION.md`](docs/DIRECTION.md), and [`AGENTS.md`](AGENTS.md) before changing code.
Protocol or store changes also require [`docs/ROUNDTRIPS.md`](docs/ROUNDTRIPS.md).

## Build

Install Git, `protoc`, `just`, Docker with Compose, and the Rust toolchain pinned by `rust-toolchain.toml`. Then:

```sh
export RUSTUP_TOOLCHAIN=1.97.1
cargo build --workspace
```

Tests use `/dev/null` as their global Git config through `.cargo/config.toml`; do not remove that isolation.

## Test tiers

Run the smallest relevant tier while developing and all required gates before submitting:

```sh
just test                                      # fast unit and integration tier
just e2e                                       # real Git smart-HTTP flow
cargo test -p gitcask-server --test sim        # fault-injection simulation
docker compose up -d --wait rustfs
docker compose run --rm create-bucket
scripts/smoke.sh . 8090                        # full server/gate/rustfs smoke test
```

Every change must also pass:

```sh
just warnings
scripts/clippy-count.sh
```

The repository intentionally carries historical pedantic Clippy warnings. The rule is no regression against
the target branch, not `-D warnings`: record the counter for the target branch and ensure the proposed change is
at or below it. Do not add `#[allow(...)]` merely to pass the gate or mix unrelated lint cleanup into a change.
`just ci` runs warnings, the checked-in Clippy baseline, the fast tests, and e2e.

Run the smoke test for changes to smart HTTP, publish, sync, authentication, Compose, or the first-run path.
Run `just test-s3` for object-store contract changes. Protocol changes must state the before/after critical-path
bucket round trips and update `docs/ROUNDTRIPS.md`.

## Changes and commits

- Keep each commit focused on one idea and use an imperative subject.
- Update tests, configuration examples, and the single authoritative document for any behavior you change.
- Do not add compatibility aliases or deprecated shapes before 1.0; remove the old shape in the same change.
- Do not commit generated attribution trailers, session URLs, credentials, or user repository data.
- Preserve the append-only WAL/protobuf compatibility rules in `AGENTS.md`.

## Developer Certificate of Origin

Contributions use the [Developer Certificate of Origin 1.1](https://developercertificate.org/), not a Contributor
License Agreement. Sign off every commit with:

```sh
git commit -s
```

The `Signed-off-by` line certifies that you have the right to submit the contribution under this repository's
Apache-2.0 license. A CLA is intentionally not required.

## Pull requests

Explain the user-visible outcome, the design decision or invariant involved, and every verification command you
ran. Include round-trip counts for bucket protocols and call out any test you could not run. Small, reviewable
pull requests are strongly preferred.
