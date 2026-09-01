#!/usr/bin/env bash
# Maintainer drain benchmark: N repos pushed with the maintainer OFF, then drained with W workers.
#
# Usage:
#   scripts/drain-bench.sh <repo-root> <N> <W> <port>
#   EVICT=30s COLD=1 scripts/drain-bench.sh <repo-root> <N> <W> <port>
set -uo pipefail
ROOT=$1; N=$2; W=$3; PORT=$4
export RUSTUP_TOOLCHAIN=1.97.1 AWS_ACCESS_KEY_ID=gitcask-dev AWS_SECRET_ACCESS_KEY=gitcask-dev-secret
BIN=$ROOT/target/debug/gitcask; X=$(mktemp -d); CACHE=$X/cache; TAG=bench$W
mk_cfg() { # $1 roles  $2 out
  sed -e "s|dir = \"/tmp/gitcask\"|dir = \"$CACHE\"\nevict_idle_after = \"${EVICT:-24h}\"\nevict_interval = \"10s\"|" \
      -e 's|auto_create_on_push = true|auto_create_on_push = false|' \
      -e "s|^roles = \[\]|roles = $1|" \
      "$ROOT/gitcask.standalone.toml" > "$2"
  printf '\n[maintenance]\nworkers = %s\ninterval = "1s"\n' "$W" >> "$2"
}
start() { PORT=$PORT "$BIN" serve --config "$1" >>"$X/server.log" 2>&1 & SPID=$!; for i in $(seq 1 60); do curl -sf http://127.0.0.1:$PORT/healthz >/dev/null && return; sleep 0.5; done; echo "server failed"; tail -5 "$X/server.log"; exit 1; }
stop() { kill $SPID 2>/dev/null; wait $SPID 2>/dev/null; }
metric() { curl -s http://127.0.0.1:$PORT/metrics | awk -v k="$1" '$0 ~ "^"k {s+=$NF} END{print s+0}'; }

echo "== W=$W N=$N: seed (maintainer off) =="
mk_cfg '["serve"]' $X/serve.toml; start $X/serve.toml
git init -q $X/src && (cd $X/src && echo x > f && git add f && git -c user.name=t -c user.email=t@t commit -q -m one)
t0=$(date +%s)
seq 1 $N | xargs -P 8 -I{} sh -c "curl -sf -X PUT http://127.0.0.1:$PORT/$TAG/r{}.git >/dev/null && cd $X/src && git push -q http://127.0.0.1:$PORT/$TAG/r{}.git HEAD:main 2>/dev/null"
echo "  seeded in $(( $(date +%s) - t0 ))s; pending=$(metric gitcask_pending_markers)"
if [ -n "${COLD:-}" ]; then sleep 75; echo "  cache dirs after evict wait: $(find $CACHE -maxdepth 2 -mindepth 2 -type d 2>/dev/null | wc -l | tr -d " ")"; fi
stop

echo "== W=$W: drain ==  workdir=$X"
mk_cfg '["maintain"]' $X/maint.toml; start $X/maint.toml
t0=$(date +%s); samples=0; busy_sum=0; cpu_sum=0
while :; do
  sleep 2
  p=$(curl -s http://127.0.0.1:$PORT/metrics | awk '/^gitcask_pending_markers/ {print $NF; exit}')
  b=$(curl -s http://127.0.0.1:$PORT/metrics | awk '/^gitcask_maintain_workers_busy/ {s+=$NF} END{print s+0}')
  c=$(ps -o %cpu= -p $SPID | tr -d ' ')
  samples=$((samples+1)); busy_sum=$(python3 -c "print($busy_sum+${b:-0})"); cpu_sum=$(python3 -c "print($cpu_sum+${c:-0})")
  done_repos=$(curl -s http://127.0.0.1:$PORT/metrics | awk '/^gitcask_maintain_pass_repos/ {print $NF; exit}')
  el=$(( $(date +%s) - t0 ))
  [ $((el % 20)) -lt 2 ] && echo "  t=${el}s pending=$p busy=$b cpu=${c}%"
  left=$(docker run --rm --network host -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_DEFAULT_REGION=us-east-1 amazon/aws-cli:latest --endpoint-url http://127.0.0.1:9000 s3api list-objects-v2 --bucket gitcask-test --prefix pending/$TAG/ --max-keys 1 --query KeyCount --output text 2>/dev/null)
  [ "$left" = "0" ] && break
  [ $el -gt 1800 ] && { echo "  TIMEOUT"; break; }
done
el=$(( $(date +%s) - t0 ))
echo "== W=$W RESULT: drained $N in ${el}s = $(python3 -c "print(round($N/max($el,1),1))") repos/s; avg busy=$(python3 -c "print(round($busy_sum/max($samples,1),1))")/$W; avg cpu=$(python3 -c "print(round($cpu_sum/max($samples,1)))")%"
echo "  store ops during drain:"; curl -s http://127.0.0.1:$PORT/metrics | grep '^gitcask_store_requests_total' | sed 's/^/    /'
stop
echo "  cleanup"; seq 1 $N | xargs -P 8 -I{} curl -sf -X DELETE http://127.0.0.1:$PORT/$TAG/r{}.git >/dev/null 2>&1
mk_cfg '["serve"]' $X/serve.toml; start $X/serve.toml; seq 1 $N | xargs -P 8 -I{} curl -sf -X DELETE http://127.0.0.1:$PORT/$TAG/r{}.git >/dev/null; stop
echo "  log errors:"; grep -ciE 'error|panic' $X/server.log
