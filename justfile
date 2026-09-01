# gitcask justfile — local dev and test targets.

# `timeout` is GNU coreutils: absent on macOS, where every wrapped recipe otherwise dies with
# `sh: timeout: command not found` (exit 127) before a single test runs — pushing contributors
# onto the broad `cargo test --workspace` that AGENTS.md forbids. Prefer timeout, then gtimeout
# (brew install coreutils), else run unwrapped: no watchdog is better than no tests.
t5 := `if command -v timeout >/dev/null 2>&1; then echo "timeout 300"; elif command -v gtimeout >/dev/null 2>&1; then echo "gtimeout 300"; else echo ""; fi`
t10 := `if command -v timeout >/dev/null 2>&1; then echo "timeout 600"; elif command -v gtimeout >/dev/null 2>&1; then echo "gtimeout 600"; else echo ""; fi`
t15 := `if command -v timeout >/dev/null 2>&1; then echo "timeout 900"; elif command -v gtimeout >/dev/null 2>&1; then echo "gtimeout 900"; else echo ""; fi`
clippy_baseline := "1092"

# Default: show available targets.
default:
    @just --list

# Local dev = one authenticated gitcask at http://gitcask.localhost:$PORT
# (default 8080), with every role (serve, maintain, events) against local rustfs.
# `config` defaults to gitcask.standalone.toml; point it at a real bucket by editing [store] there. The rustfs
# keys come from the environment (AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY; compose.yaml fixes them).
dev-local config="gitcask.standalone.toml":
    #!/usr/bin/env bash
    set -euo pipefail
    export AWS_ACCESS_KEY_ID="${AWS_ACCESS_KEY_ID:-gitcask-dev}" AWS_SECRET_ACCESS_KEY="${AWS_SECRET_ACCESS_KEY:-gitcask-dev-secret}"
    rustfs_port="${GITCASK_RUSTFS_PORT:-19000}"
    if ! curl -sf "http://127.0.0.1:${rustfs_port}/minio/health/live" >/dev/null 2>&1; then
        echo "rustfs not running on :${rustfs_port} — starting it (just dev-store)"
        just dev-store
    fi
    cargo build --release --bin gitcask-server
    port="${PORT:-8080}"
    echo "→ http://gitcask.localhost:${port}/  (single process, config {{config}}, store rustfs :${rustfs_port})"
    env -u PORT \
      GITCASK__SERVER__LISTEN="127.0.0.1:${port}" \
      GITCASK__SERVER__PUBLIC_URL="http://gitcask.localhost:${port}" \
      ./target/release/gitcask-server --config {{config}}

# Start rustfs (S3-compatible) for local dev via podman compose (rootless, no daemon group needed;
# `podman compose` drives compose.yaml through the docker-compose binary dev.yml installs).
# `podman compose` talks to the podman API socket; rootless nix podman has no systemd unit for it, so
# `podman system service` is started (detached, idle-timeout 0) when the socket is missing.
dev-store:
    #!/usr/bin/env bash
    set -euo pipefail
    # nix podman ships no /etc/containers: give the user a signature policy + registry search list once.
    cdir="${XDG_CONFIG_HOME:-$HOME/.config}/containers"; mkdir -p "$cdir"
    [ -f "$cdir/policy.json" ] || printf '{"default":[{"type":"insecureAcceptAnything"}]}\n' > "$cdir/policy.json"
    [ -f "$cdir/registries.conf" ] || printf 'unqualified-search-registries = ["docker.io"]\n' > "$cdir/registries.conf"
    sock="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/podman/podman.sock"
    if [ ! -S "$sock" ]; then
        echo "starting rootless podman API socket at $sock"
        mkdir -p "$(dirname "$sock")"
        setsid nohup podman system service --time=0 "unix://$sock" >/tmp/gitcask-podman-service.log 2>&1 < /dev/null &
        for _ in $(seq 1 50); do [ -S "$sock" ] && break; sleep 0.2; done
        [ -S "$sock" ] || { echo "podman API socket did not appear; see /tmp/gitcask-podman-service.log"; exit 1; }
    fi
    podman compose up -d rustfs
    echo "Waiting for rustfs to be healthy..."
    podman compose run --rm create-bucket
    rustfs_port="${GITCASK_RUSTFS_PORT:-19000}"
    rustfs_console_port="${GITCASK_RUSTFS_CONSOLE_PORT:-19001}"
    echo "rustfs is running on http://127.0.0.1:${rustfs_port} (console :${rustfs_console_port})"
    echo "Credentials: gitcask-dev / gitcask-dev-secret"
    echo "Bucket: gitcask-test"

# Stop rustfs.
dev-store-stop:
    podman compose down

# --- tests -------------------------------------------------------------------
# Tiers (all hermetic: in-memory store, tempdir caches, real `git` binary):
#   test       fast tier, < 30 s: every unit/integration test not marked #[ignore]
#   test-slow  benches/soak: #[ignore]d tests (20k-ref push, 466k-ref render, ...)
#   test-s3    store contract against local rustfs (just dev-store)

# Fast hermetic tier (< 30 s): every test not marked #[ignore].
# Fast tier (default, < 1 min): unit tests + the quick integration suites.
# Never run `cargo test --workspace --no-fail-fast` interactively: a single
# hung test blocks for the whole timeout. Use `just e2e` / `just ci` below.
test:
    {{t5}} cargo test --workspace --lib --bins
    {{t5}} cargo test -p gitcask-store --features testing --tests
    {{t5}} cargo test -p gitcask-git -p gitcask-wal --tests
    {{t5}} cargo test -p gitcask-server --test web_api --test api_v1 --test static_http --test maintain --test routing_prefix --test drain --test retryable_store

# Smart-HTTP end-to-end against real git (≈ 20 s) — run when touching smart.rs/receive/upload-pack/wal.
e2e *ARGS:
    {{t10}} cargo test -p gitcask-server --test e2e {{ARGS}}

# Zero rustc warnings, workspace-wide, all targets (tests, benches, examples).
# Done by grepping the normal build instead of RUSTFLAGS=-D warnings, which would
# change every crate's fingerprint and force full rebuilds in every shell.
warnings:
    #!/usr/bin/env bash
    set -uo pipefail
    # A command substitution that fails does NOT abort under `set -uo pipefail` (no -e), so a
    # workspace that does not compile used to fall through to "no rustc warnings" and exit 0 —
    # the preflight passing on a broken tree. Check the build's status before grepping it.
    if ! out="$({{t15}} cargo build --workspace --all-targets 2>&1)"; then
        printf '%s\n' "$out"
        echo; echo "cargo build failed — fix the errors above"; exit 1
    fi
    if printf '%s\n' "$out" | grep -qE '^warning: (unused|function|variable|field|method|struct|enum|never|dead|irrefutable|unreachable|value assigned|deprecated|trait|type|constant|static|associated)'; then
        printf '%s\n' "$out" | grep -E '^warning' -A4 | grep -vE '^warning: `gitcask-[a-z]+`'
        echo; echo "rustc warnings present — fix them (just warnings is part of just ci and the deploy preflight)"; exit 1
    fi
    echo "no rustc warnings"

# Clippy, workspace-wide and all targets. The tree carries historical warnings, so the gate
# is deterministic no-regression rather than -D warnings. Lower this baseline whenever main
# removes warnings; never raise it to make a change pass.
clippy:
    #!/usr/bin/env bash
    set -euo pipefail
    count=$(scripts/clippy-count.sh)
    baseline={{clippy_baseline}}
    echo "clippy warnings: $count (baseline: $baseline)"
    if (( count > baseline )); then
        echo "clippy warning regression: $count > $baseline"
        exit 1
    fi

# Everything that must be green before a merge (what CI runs).
ci: warnings clippy test e2e

# Slow tier: #[ignore]d benches/soaks (20k-ref push, 466k-ref render, ...).
test-slow:
    cargo test --workspace -- --ignored --nocapture

# Run gitcask-store contract tests against memory only.
store-test:
    cargo test -p gitcask-store --features testing --test contract -- memory_contract

# Run gitcask-store contract tests against rustfs (requires `just dev-store` first).
# Store contract against local rustfs (run `just dev-store` first).
test-s3: store-test-s3

store-test-s3:
    GITCASK_TEST_S3_ENDPOINT=http://127.0.0.1:9000 \
    GITCASK_TEST_BUCKET=gitcask-test \
    AWS_ACCESS_KEY_ID=gitcask-dev \
    AWS_SECRET_ACCESS_KEY=gitcask-dev-secret \
    cargo test -p gitcask-store --features testing --test contract -- --nocapture

# Run all gitcask-store tests (memory + S3 if env set).
store-test-all:
    cargo test -p gitcask-store --features testing
