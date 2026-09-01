#!/usr/bin/env bash
# Deterministic clippy warning count for the workspace.
# Counts compiler-message diagnostics of level "warning" from cargo's JSON stream, which cargo
# replays for cached (fresh) crates too — so the number does not depend on what is in target/.
# Prints "<total>" on stdout and a per-crate breakdown on stderr.
set -uo pipefail
# clippy may exit non-zero on deny-level lints in the baseline; we only count.
cd "$(dirname "$0")/.."
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN_OVERRIDE:-1.97.1}"
# Cached (fresh) workspace crates are not re-diagnosed even in the JSON stream, so force a
# re-check of every workspace crate; dependencies stay cached.
pkgs=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import sys,json; print(" ".join("-p "+p["name"] for p in json.load(sys.stdin)["packages"]))')
# shellcheck disable=SC2086
cargo clean $pkgs >/dev/null 2>&1 || true
{ cargo clippy --workspace --all-targets --message-format=json 2>/dev/null || true; } | python3 -c '
import sys, json, collections
per = collections.Counter()
total = 0
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") != "compiler-message":
        continue
    msg = m["message"]
    if msg.get("level") != "warning":
        continue
    text = msg.get("message", "")
    if text.startswith("`") and "generated" in text:
        continue  # per-crate summary line
    pkg = m["package_id"].split("#")[-1].split("@")[0]
    if pkg[0].isdigit(): pkg = m["package_id"].split("/")[-1].split("#")[0]
    t = m["target"]
    key = pkg + " (" + ",".join(t["kind"]) + " " + t["name"] + ")"
    per[key] += 1
    total += 1
for k in sorted(per):
    print("%5d %s" % (per[k], k), file=sys.stderr)
print(total)
'
