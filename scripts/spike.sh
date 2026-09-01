#!/usr/bin/env bash
# Many-small-repositories load spike against the local rustfs development bucket.
# Usage: scripts/spike.sh <repo-root> [n=1000] [port=8093]
set -uo pipefail

ROOT=${1:?repo root}
N=${2:-1000}
PORT=${3:-8093}
case "$N:$PORT" in
    *[!0-9:]* | :* | *:) echo "n and port must be positive integers" >&2; exit 2 ;;
esac
if [ "$N" -lt 1 ] || [ "$PORT" -lt 1 ]; then
    echo "n and port must be positive integers" >&2
    exit 2
fi

ROOT=$(cd "$ROOT" && pwd)
export RUSTUP_TOOLCHAIN=1.97.1
export AWS_ACCESS_KEY_ID=gitcask-dev
export AWS_SECRET_ACCESS_KEY=gitcask-dev-secret
export AWS_DEFAULT_REGION=us-east-1
export GIT_CONFIG_GLOBAL=/dev/null

WORK=$(mktemp -d)
TABLE=$WORK/table.tsv
BIN=$ROOT/target/debug/gitcask
BASE=http://127.0.0.1:$PORT
RESULTS_DIR=$ROOT/spike-results
EVICT_IDLE_SECONDS=30
EVICT_INTERVAL_SECONDS=10
SPID=
FAILURES=0
LAST_IDLE=0
LAST_IDLE_COMPLETE=0
LAST_JSON=
PACK_CONVERGENCE_SECONDS=0
PACK_CONVERGENCE_LIVE=-1
PACK_CONVERGENCE_BUCKET=-1
PACK_CONVERGENCE_COMPACTIONS=0
PACK_CONVERGENCE_ERROR=

now_ms() {
    python3 -c 'import time; print(time.time_ns() // 1_000_000)'
}

elapsed_seconds() {
    awk -v start="$1" -v end="$2" 'BEGIN { printf "%.3f", (end-start)/1000 }'
}

ratio() {
    awk -v numerator="$1" -v denominator="$2" 'BEGIN { if (denominator == 0) print 0; else printf "%.2f", numerator/denominator }'
}

throughput_scale() {
    awk -v baseline_n="$1" -v baseline_seconds="$2" -v requested_n="$3" -v requested_seconds="$4" \
        'BEGIN { if (baseline_seconds == 0 || requested_seconds == 0) print 0; else printf "%.2f", (requested_n/requested_seconds)/(baseline_n/baseline_seconds) }'
}

percentile() {
    python3 - "$1" "$2" <<'PY'
import math
import sys

values = []
try:
    with open(sys.argv[1], encoding="utf-8") as source:
        values = sorted(float(line.strip()) for line in source if line.strip())
except FileNotFoundError:
    pass
if not values:
    print(0)
else:
    rank = max(0, math.ceil(float(sys.argv[2]) * len(values)) - 1)
    print(f"{values[rank]:.1f}")
PY
}

s3api() {
    docker run --rm --network host \
        -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_DEFAULT_REGION \
        amazon/aws-cli:latest --endpoint-url http://127.0.0.1:9000 s3api "$@" 2>/dev/null
}

s3() {
    docker run --rm --network host \
        -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_DEFAULT_REGION \
        amazon/aws-cli:latest --endpoint-url http://127.0.0.1:9000 s3 "$@"
}

prefix_count() {
    local count
    count=$(s3api list-objects-v2 --bucket gitcask-test --prefix "$1" --query KeyCount --output text) || count=0
    case "$count" in
        '' | None) echo 0 ;;
        *) echo "$count" ;;
    esac
}

wal_pack_count() {
    local prefix=$1 listing count
    if ! listing=$(s3 ls "s3://gitcask-test/$prefix" --recursive); then
        echo "ERROR: failed to list bucket packs under $prefix" >&2
        return 1
    fi
    if ! count=$(printf '%s\n' "$listing" | awk '$4 ~ /\/wal\/.*\.pack$/ { count++ } END { print count+0 }'); then
        echo "ERROR: failed to count bucket packs under $prefix" >&2
        return 1
    fi
    case "$count" in
        '' | *[!0-9]*)
            echo "ERROR: invalid bucket pack count '$count' under $prefix" >&2
            return 1
            ;;
    esac
    echo "$count"
}

wait_pack_convergence() {
    local prefix=$1 overview_path=$2 timeout_seconds=$3 compactions_before=$4
    local started now elapsed_ms live bucket compactions_after compactions
    PACK_CONVERGENCE_SECONDS=0
    PACK_CONVERGENCE_LIVE=-1
    PACK_CONVERGENCE_BUCKET=-1
    PACK_CONVERGENCE_COMPACTIONS=0
    PACK_CONVERGENCE_ERROR=
    started=$(now_ms)
    # A push-loop boundary is not a GC boundary: the cursor becomes due only
    # after the last COMPACT is published, then GC waits behind fsck as the
    # maintainer's lowest-priority unit. Observe that asynchronous convergence.
    while :; do
        if ! live=$(curl -sf "$BASE$overview_path" 2>/dev/null | jq -er '.packs.live'); then
            PACK_CONVERGENCE_ERROR="live pack query failed"
            return 2
        fi
        case "$live" in
            '' | *[!0-9]*)
                PACK_CONVERGENCE_ERROR="invalid live pack count '$live'"
                return 2
                ;;
        esac
        if ! bucket=$(wal_pack_count "$prefix"); then
            PACK_CONVERGENCE_ERROR="bucket pack count failed"
            return 2
        fi
        compactions_after=$(compact_total)
        case "$compactions_after" in
            '' | *[!0-9]*)
                PACK_CONVERGENCE_ERROR="invalid compaction count '$compactions_after'"
                return 2
                ;;
        esac
        compactions=$((compactions_after - compactions_before))
        now=$(now_ms)
        elapsed_ms=$((now - started))
        PACK_CONVERGENCE_SECONDS=$(elapsed_seconds "$started" "$now")
        PACK_CONVERGENCE_LIVE=$live
        PACK_CONVERGENCE_BUCKET=$bucket
        PACK_CONVERGENCE_COMPACTIONS=$compactions
        if [ "$compactions" -ge 1 ] && [ "$bucket" -eq "$live" ]; then
            return 0
        fi
        if [ "$elapsed_ms" -ge $((timeout_seconds * 1000)) ]; then
            return 1
        fi
        sleep 1
    done
}

clean_spike_prefixes() {
    s3 rm --recursive s3://gitcask-test/repos/spike/ >/dev/null 2>&1 || true
    s3 rm --recursive s3://gitcask-test/pending/spike/ >/dev/null 2>&1 || true
}

stop_server() {
    if [ -z "${SPID:-}" ]; then
        return
    fi
    kill -TERM "$SPID" 2>/dev/null || true
    for _ in $(seq 1 200); do
        if ! kill -0 "$SPID" 2>/dev/null; then
            wait "$SPID" 2>/dev/null || true
            SPID=
            return
        fi
        sleep 0.1
    done
    kill -KILL "$SPID" 2>/dev/null || true
    wait "$SPID" 2>/dev/null || true
    SPID=
}

start_server() {
    local cfg=$1 log=$2
    PORT=$PORT "$BIN" serve --config "$cfg" >>"$log" 2>&1 &
    SPID=$!
    for _ in $(seq 1 120); do
        if curl -sf "$BASE/healthz" >/dev/null 2>&1; then
            return 0
        fi
        if ! kill -0 "$SPID" 2>/dev/null; then
            echo "server exited during startup; log: $log" >&2
            tail -30 "$log" >&2
            SPID=
            return 1
        fi
        sleep 0.25
    done
    echo "server did not become healthy; log: $log" >&2
    tail -30 "$log" >&2
    stop_server
    return 1
}

metrics() {
    curl -sf "$BASE/metrics" 2>/dev/null || true
}

store_total() {
    metrics | awk '$1 ~ /^gitcask_store_requests(_total)?\{/ { total += $2 } END { printf "%.0f\n", total }'
}

store_outcome_total() {
    local outcome=$1
    metrics | awk -v label="outcome=\"$outcome\"" \
        '$1 ~ /^gitcask_store_requests(_total)?\{/ && index($1, label) { total += $2 } END { printf "%.0f\n", total }'
}

pending_gauge() {
    metrics | awk '$1 ~ /^gitcask_pending_markers(\{|$)/ { value += $2; found=1 } END { if (found) printf "%.0f\n", value; else print 0 }'
}

compact_total() {
    metrics | awk '$1 ~ /^gitcask_maintain_units_total\{/ && $1 ~ /kind="compact"/ && $1 ~ /outcome="ok"/ { total += $2 } END { printf "%.0f\n", total }'
}

add_row() {
    printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "$4" >>"$TABLE"
}

wait_pending_empty() {
    local timeout_seconds=$1 started now count
    started=$(now_ms)
    while :; do
        count=$(prefix_count pending/spike/)
        if [ "$count" -eq 0 ]; then
            return 0
        fi
        now=$(now_ms)
        if [ $(((now - started) / 1000)) -ge "$timeout_seconds" ]; then
            return 1
        fi
        sleep 1
    done
}

make_configs() {
    local run_work=$1 cache=$2 all_cfg=$3 serve_cfg=$4 max_repos=$5
    mkdir -p "$run_work" "$cache"
    sed \
        -e "s|dir = \"/tmp/gitcask\"|dir = \"$cache\"\nevict_idle_after = \"${EVICT_IDLE_SECONDS}s\"\nevict_interval = \"${EVICT_INTERVAL_SECONDS}s\"|" \
        -e 's|auto_create_on_push = true|auto_create_on_push = false|' \
        "$ROOT/gitcask.standalone.toml" >"$all_cfg"
    printf '\n[maintenance]\ninterval = "5s"\nworkers = 8\nmax_repos_per_pass = %s\nfsck_interval = "0s"\n' "$max_repos" >>"$all_cfg"
    printf '\n[compaction]\nretention_superseded = "0s"\n' >>"$all_cfg"
    sed 's|roles = \[\]|roles = ["serve"]|' "$all_cfg" >"$serve_cfg"
}

run_case() {
    local count=$1 label=$2
    LAST_IDLE=0
    LAST_IDLE_COMPLETE=0
    LAST_JSON=
    local run_work=$WORK/$label-$count
    local cache=$run_work/cache log=$run_work/server.log
    local all_cfg=$run_work/all.toml serve_cfg=$run_work/serve.toml source=$run_work/source
    local create_status=OK push_status=OK maintain_status=OK idle_status=OK evict_status=OK compact_status=OK delete_status=OK
    local started ended seconds before after requests rate request_rate code bad_count
    local create_seconds create_rate create_store_per_repo push_seconds push_store_per_push maintainer_store_per_repo
    local push_times=$run_work/push-times.txt clone_times=$run_work/clone-times.txt
    local push_p50 push_p99 clone_p50 cas_before cas_after cas_retries
    local pending_immediate pending_metric drain_seconds pending_left drain_timeout drain_complete=1
    local idle_requests idle_seconds=300 cache_repos clone_count concentrated_before concentrated_after concentrated_requests
    local compactions_before compactions live_packs bucket_packs concentrated_bad=0
    local pack_convergence_seconds=0 pack_convergence_result=not-run pack_convergence_error= convergence_rc=0
    local repos_left pending_objects delete_seconds timestamp json

    echo "== n=$count: prepare =="
    make_configs "$run_work" "$cache" "$all_cfg" "$serve_cfg" "$count"
    clean_spike_prefixes
    git init -q "$source"
    git -C "$source" config user.name spike
    git -C "$source" config user.email spike@gitcask.local
    printf 'one\n' >"$source/payload.txt"
    git -C "$source" add payload.txt
    git -C "$source" commit -q -m one

    if ! start_server "$serve_cfg" "$log"; then
        add_row "$count" infrastructure "server startup failed" FAIL
        FAILURES=$((FAILURES + 1))
        LAST_IDLE=0
        LAST_JSON=
        return
    fi

    echo "== n=$count: 1/7 create repositories =="
    before=$(store_total)
    started=$(now_ms)
    bad_count=0
    for i in $(seq 1 "$count"); do
        code=$(curl -sS -o /dev/null -w '%{http_code}' -X PUT "$BASE/spike/r$i.git") || code=000
        case "$code" in 200 | 201) ;; *) bad_count=$((bad_count + 1)) ;; esac
    done
    ended=$(now_ms)
    after=$(store_total)
    seconds=$(elapsed_seconds "$started" "$ended")
    requests=$((after - before))
    rate=$(ratio "$count" "$seconds")
    request_rate=$(ratio "$requests" "$count")
    create_seconds=$seconds
    create_rate=$rate
    create_store_per_repo=$request_rate
    if [ "$bad_count" -ne 0 ]; then create_status=FAIL; FAILURES=$((FAILURES + 1)); fi
    add_row "$count" create "${seconds}s; ${rate} repos/s; ${request_rate} store/repo; failed=$bad_count" "$create_status"

    echo "== n=$count: 2/7 first push (parallel 8) =="
    before=$(store_total)
    cas_before=$(store_outcome_total precondition_failed)
    started=$(now_ms)
    python3 - "$source" "$BASE" "$count" "$push_times" <<'PY'
import concurrent.futures
import os
import subprocess
import sys
import time

source, base, count, output = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]

def push(index):
    started = time.monotonic_ns()
    result = subprocess.run(
        ["git", "-C", source, "push", "-q", f"{base}/spike/r{index}.git", "HEAD:main"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        env=os.environ,
    )
    elapsed = (time.monotonic_ns() - started) / 1_000_000
    return elapsed, result.returncode, result.stderr.strip()

failures = []
durations = []
with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
    futures = {pool.submit(push, index): index for index in range(1, count + 1)}
    for future in concurrent.futures.as_completed(futures):
        elapsed, returncode, stderr = future.result()
        if returncode == 0:
            durations.append(elapsed)
        else:
            failures.append((futures[future], stderr))

with open(output, "w", encoding="utf-8") as target:
    for duration in durations:
        target.write(f"{duration:.3f}\n")
if failures:
    for index, stderr in failures[:10]:
        print(f"push r{index} failed: {stderr}", file=sys.stderr)
raise SystemExit(bool(failures))
PY
    bad_count=$?
    if [ "$bad_count" -ne 0 ]; then
        push_status=FAIL
    fi
    ended=$(now_ms)
    after=$(store_total)
    cas_after=$(store_outcome_total precondition_failed)
    seconds=$(elapsed_seconds "$started" "$ended")
    requests=$((after - before))
    request_rate=$(ratio "$requests" "$count")
    push_seconds=$seconds
    push_store_per_push=$request_rate
    push_p50=$(percentile "$push_times" 0.50)
    push_p99=$(percentile "$push_times" 0.99)
    cas_retries=$((cas_after - cas_before))
    if [ "$(wc -l <"$push_times" | tr -d ' ')" -ne "$count" ] || [ "$cas_retries" -ne 0 ]; then
        push_status=FAIL
        FAILURES=$((FAILURES + 1))
    fi
    add_row "$count" first-push "${seconds}s; p50=${push_p50}ms; p99=${push_p99}ms; ${request_rate} store/push; 412=$cas_retries" "$push_status"

    echo "== n=$count: 3/7 drain pending markers =="
    stop_server
    started=$(now_ms)
    if ! start_server "$all_cfg" "$log"; then
        add_row "$count" maintainer "server restart failed" FAIL
        FAILURES=$((FAILURES + 1))
        LAST_IDLE=0
        LAST_JSON=
        return
    fi
    sleep 0.25
    pending_metric=$(pending_gauge)
    pending_immediate=$pending_metric
    drain_timeout=$(((count + 4) / 5))
    if [ "$drain_timeout" -lt 600 ]; then drain_timeout=600; fi
    if ! wait_pending_empty "$drain_timeout"; then
        maintain_status=FAIL
        drain_complete=0
        FAILURES=$((FAILURES + 1))
    fi
    ended=$(now_ms)
    # Allow the next empty pass to publish a zero pending-marker gauge.
    for _ in $(seq 1 30); do
        [ "$(pending_gauge)" -eq 0 ] && break
        sleep 0.2
    done
    after=$(store_total)
    drain_seconds=$(elapsed_seconds "$started" "$ended")
    pending_left=$(prefix_count pending/spike/)
    requests=$after
    request_rate=$(ratio "$requests" "$count")
    maintainer_store_per_repo=$request_rate
    if [ "$pending_left" -ne 0 ]; then maintain_status=FAIL; fi
    add_row "$count" maintainer "pending gauge=${pending_immediate}; drain=${drain_seconds}s; ${request_rate} store/repo; left=$pending_left" "$maintain_status"

    echo "== n=$count: 4/7 idle maintainer cost (300s) =="
    if [ "$drain_complete" -eq 1 ]; then
        before=$(store_total)
        sleep 300
        after=$(store_total)
        idle_requests=$((after - before))
        add_row "$count" idle-5m "store requests=$idle_requests" "$idle_status"
    else
        idle_status=FAIL
        idle_requests=0
        idle_seconds=0
        add_row "$count" idle-5m "skipped: pending-marker drain did not finish" "$idle_status"
        FAILURES=$((FAILURES + 1))
    fi

    echo "== n=$count: 5/7 idle eviction and 20 cold clones =="
    sleep $((EVICT_IDLE_SECONDS + EVICT_INTERVAL_SECONDS + 5))
    if [ -d "$cache/spike" ]; then
        cache_repos=$(find "$cache/spike" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')
    else
        cache_repos=0
    fi
    if [ "$cache_repos" -ne 0 ]; then
        evict_status=FAIL
        FAILURES=$((FAILURES + 1))
    fi
    stop_server
    rm -rf "$cache"
    mkdir -p "$cache"
    if ! start_server "$all_cfg" "$log"; then
        add_row "$count" evict-clone "server restart failed" FAIL
        FAILURES=$((FAILURES + 1))
        LAST_IDLE=$idle_requests
        LAST_JSON=
        return
    fi
    clone_count=$count
    if [ "$clone_count" -gt 20 ]; then clone_count=20; fi
    python3 - "$BASE" "$run_work" "$clone_count" "$clone_times" <<'PY'
import os
import subprocess
import sys
import time

base, work, count, output = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
durations = []
failures = 0
for index in range(1, count + 1):
    started = time.monotonic_ns()
    result = subprocess.run(
        ["git", "clone", "-q", f"{base}/spike/r{index}.git", f"{work}/clone-{index}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
        env=os.environ,
    )
    elapsed = (time.monotonic_ns() - started) / 1_000_000
    if result.returncode == 0:
        durations.append(elapsed)
    else:
        failures += 1
        print(f"clone r{index} failed: {result.stderr.strip()}", file=sys.stderr)
with open(output, "w", encoding="utf-8") as target:
    for duration in durations:
        target.write(f"{duration:.3f}\n")
raise SystemExit(bool(failures))
PY
    bad_count=$?
    clone_p50=$(percentile "$clone_times" 0.50)
    if [ "$(wc -l <"$clone_times" | tr -d ' ')" -ne "$clone_count" ] || [ "$bad_count" -ne 0 ]; then
        evict_status=FAIL
        FAILURES=$((FAILURES + 1))
    fi
    add_row "$count" evict-clone "cache repos after $((EVICT_IDLE_SECONDS + EVICT_INTERVAL_SECONDS + 5))s=$cache_repos; cold clone p50=${clone_p50}ms ($clone_count repos)" "$evict_status"

    echo "== n=$count: 6/7 concentrated 200-push session =="
    concentrated_before=$(store_total)
    compactions_before=$(compact_total)
    for i in $(seq 1 200); do
        printf '%s\n' "$i" >>"$source/payload.txt"
        if ! git -C "$source" add payload.txt || ! git -C "$source" commit -q -m "session $i" || \
            ! git -C "$source" push -q "$BASE/spike/r1.git" HEAD:main >>"$run_work/concentrated-push.log" 2>&1; then
            concentrated_bad=$((concentrated_bad + 1))
        fi
        if [ $((i % 25)) -eq 0 ]; then printf '  concentrated pushes: %d/200\n' "$i"; fi
    done
    if wait_pack_convergence repos/spike/r1/ /spike/r1/api/overview 120 "$compactions_before"; then
        pack_convergence_result=converged
    else
        convergence_rc=$?
        if [ "$convergence_rc" -eq 1 ]; then
            pack_convergence_result=timeout
        else
            pack_convergence_result=count-error
            pack_convergence_error=$PACK_CONVERGENCE_ERROR
        fi
    fi
    live_packs=$PACK_CONVERGENCE_LIVE
    bucket_packs=$PACK_CONVERGENCE_BUCKET
    compactions=$PACK_CONVERGENCE_COMPACTIONS
    pack_convergence_seconds=$PACK_CONVERGENCE_SECONDS
    concentrated_after=$(store_total)
    concentrated_requests=$((concentrated_after - concentrated_before))
    if [ "$concentrated_bad" -ne 0 ] || [ "$compactions" -lt 1 ] || [ "$live_packs" -ge 201 ] || [ "$convergence_rc" -ne 0 ]; then
        compact_status=FAIL
        FAILURES=$((FAILURES + 1))
    fi
    add_row "$count" concentrated-push "failed=$concentrated_bad; compactions=$compactions; live packs=$live_packs; bucket packs=$bucket_packs; pack convergence=$pack_convergence_result in ${pack_convergence_seconds}s${pack_convergence_error:+ ($pack_convergence_error)}; store requests=$concentrated_requests" "$compact_status"

    echo "== n=$count: 7/7 delete repositories =="
    started=$(now_ms)
    bad_count=0
    for i in $(seq 1 "$count"); do
        code=$(curl -sS -o /dev/null -w '%{http_code}' -X DELETE "$BASE/spike/r$i.git") || code=000
        case "$code" in 200 | 204) ;; *) bad_count=$((bad_count + 1)) ;; esac
    done
    ended=$(now_ms)
    delete_seconds=$(elapsed_seconds "$started" "$ended")
    repos_left=$(prefix_count repos/spike/)
    pending_objects=$(prefix_count pending/spike/)
    if [ "$bad_count" -ne 0 ] || [ "$repos_left" -ne 0 ] || [ "$pending_objects" -ne 0 ]; then
        delete_status=FAIL
        FAILURES=$((FAILURES + 1))
    fi
    add_row "$count" delete "${delete_seconds}s; failed=$bad_count; repos objects=$repos_left; pending objects=$pending_objects" "$delete_status"
    stop_server

    timestamp=$(date -u +%Y%m%dT%H%M%SZ)
    mkdir -p "$RESULTS_DIR"
    json=$RESULTS_DIR/spike-$count-$timestamp.json
    jq -n \
        --argjson n "$count" --arg label "$label" \
        --argjson create_seconds "$create_seconds" --argjson create_rate "$create_rate" --argjson create_store_per_repo "$create_store_per_repo" \
        --argjson push_seconds "$push_seconds" --argjson push_p50_ms "$push_p50" --argjson push_p99_ms "$push_p99" \
        --argjson push_store_per_push "$push_store_per_push" --argjson cas_precondition_failures "$cas_retries" \
        --argjson pending_immediate "$pending_immediate" --argjson pending_gauge "$pending_metric" --argjson maintainer_seconds "$drain_seconds" \
        --argjson maintainer_store_per_repo "$maintainer_store_per_repo" --argjson idle_seconds "$idle_seconds" --argjson idle_store_requests "$idle_requests" \
        --argjson eviction_wait_seconds "$((EVICT_IDLE_SECONDS + EVICT_INTERVAL_SECONDS + 5))" --argjson cache_repos_after_wait "$cache_repos" --argjson cold_clone_p50_ms "$clone_p50" \
        --argjson concentrated_push_failures "$concentrated_bad" --argjson compactions "$compactions" \
        --argjson live_packs "$live_packs" --argjson bucket_packs "$bucket_packs" \
        --argjson pack_convergence_seconds "$pack_convergence_seconds" --arg pack_convergence_result "$pack_convergence_result" --arg pack_convergence_error "$pack_convergence_error" \
        --argjson concentrated_store_requests "$concentrated_requests" \
        --argjson delete_seconds "$delete_seconds" --argjson delete_failures "$bad_count" \
        --argjson repos_prefix_objects "$repos_left" --argjson pending_prefix_objects "$pending_objects" \
        --arg create_status "$create_status" --arg push_status "$push_status" --arg maintain_status "$maintain_status" \
        --arg idle_status "$idle_status" --arg evict_status "$evict_status" --arg compact_status "$compact_status" --arg delete_status "$delete_status" \
        '{n:$n, run:$label,
          create:{seconds:$create_seconds,repos_per_second:$create_rate,store_requests_per_repo:$create_store_per_repo,status:$create_status},
          first_push:{seconds:$push_seconds,p50_ms:$push_p50_ms,p99_ms:$push_p99_ms,store_requests_per_push:$push_store_per_push,precondition_failures:$cas_precondition_failures,status:$push_status},
          maintainer:{pending_immediate:$pending_immediate,pending_gauge:$pending_gauge,seconds_to_empty:$maintainer_seconds,store_requests_per_repo:$maintainer_store_per_repo,status:$maintain_status},
          idle:{seconds:$idle_seconds,store_requests:$idle_store_requests,status:$idle_status},
          eviction_and_clone:{wait_seconds:$eviction_wait_seconds,cache_repos_after_wait:$cache_repos_after_wait,cold_clone_p50_ms:$cold_clone_p50_ms,status:$evict_status},
          concentrated_push:{pushes:200,push_failures:$concentrated_push_failures,compactions:$compactions,live_packs:$live_packs,bucket_packs:$bucket_packs,pack_convergence_seconds:$pack_convergence_seconds,pack_convergence_result:$pack_convergence_result,pack_convergence_error:$pack_convergence_error,store_requests:$concentrated_store_requests,status:$compact_status},
          delete:{seconds:$delete_seconds,failures:$delete_failures,repos_prefix_objects:$repos_prefix_objects,pending_prefix_objects:$pending_prefix_objects,status:$delete_status}}' >"$json"
    LAST_IDLE=$idle_requests
    LAST_IDLE_COMPLETE=$drain_complete
    LAST_JSON=$json
    echo "wrote $json"
}

trap stop_server EXIT INT TERM
cd "$ROOT"
echo "== build =="
if ! cargo build -p gitcask-cli --bin gitcask; then
    echo "build failed" >&2
    exit 1
fi

run_case 100 baseline
BASELINE_IDLE=$LAST_IDLE
BASELINE_IDLE_COMPLETE=$LAST_IDLE_COMPLETE
BASELINE_JSON=$LAST_JSON
run_case "$N" requested
REQUESTED_IDLE=$LAST_IDLE
REQUESTED_IDLE_COMPLETE=$LAST_IDLE_COMPLETE
REQUESTED_JSON=$LAST_JSON

echo
echo "== load spike results =="
printf 'n\tstep\tmeasurement\tstatus\n'
if command -v column >/dev/null 2>&1; then
    column -t -s "$(printf '\t')" "$TABLE"
else
    cat "$TABLE"
fi
echo "JSON: ${BASELINE_JSON:-not written} ${REQUESTED_JSON:-not written}"
echo "phase failures: $FAILURES"

if [ -n "${BASELINE_JSON:-}" ] && [ -n "${REQUESTED_JSON:-}" ]; then
    CREATE_SCALE=$(throughput_scale 100 "$(jq -r '.create.seconds' "$BASELINE_JSON")" "$N" "$(jq -r '.create.seconds' "$REQUESTED_JSON")")
    PUSH_SCALE=$(throughput_scale 100 "$(jq -r '.first_push.seconds' "$BASELINE_JSON")" "$N" "$(jq -r '.first_push.seconds' "$REQUESTED_JSON")")
    DRAIN_SCALE=$(throughput_scale 100 "$(jq -r '.maintainer.seconds_to_empty' "$BASELINE_JSON")" "$N" "$(jq -r '.maintainer.seconds_to_empty' "$REQUESTED_JSON")")
    DELETE_SCALE=$(throughput_scale 100 "$(jq -r '.delete.seconds' "$BASELINE_JSON")" "$N" "$(jq -r '.delete.seconds' "$REQUESTED_JSON")")
    echo "THROUGHPUT SCALE (n=100 대비 n=$N, requested/baseline; linear ≈1): create=$CREATE_SCALE push=$PUSH_SCALE drain=$DRAIN_SCALE delete=$DELETE_SCALE"
fi

if [ "$BASELINE_IDLE_COMPLETE" -eq 1 ] && [ "$REQUESTED_IDLE_COMPLETE" -eq 1 ]; then
    IDLE_DIFF=$((BASELINE_IDLE - REQUESTED_IDLE))
    if [ "$IDLE_DIFF" -lt 0 ]; then IDLE_DIFF=$((-IDLE_DIFF)); fi
    IDLE_TOLERANCE=$((BASELINE_IDLE / 10))
    if [ "$IDLE_TOLERANCE" -lt 1 ]; then IDLE_TOLERANCE=1; fi
    if [ "$IDLE_DIFF" -le "$IDLE_TOLERANCE" ]; then
        echo "IDLE COST OK: n=100 $BASELINE_IDLE requests | n=$N $REQUESTED_IDLE requests (tolerance ±10%)"
    else
        echo "IDLE COST FAIL: n=100 $BASELINE_IDLE requests | n=$N $REQUESTED_IDLE requests (tolerance ±10%)"
    fi
else
    echo "IDLE COST FAIL: skipped because at least one pending-marker drain did not finish"
fi
