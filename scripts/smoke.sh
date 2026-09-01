#!/usr/bin/env bash
# gitcask local smoke test against rustfs (docker compose in the repo).
# Usage: scripts/smoke.sh <repo-root> [port]   (needs: docker compose up -d rustfs && docker compose run --rm create-bucket)
set -uo pipefail
ROOT=${1:?repo root}; PORT=${2:-8090}
export RUSTUP_TOOLCHAIN=1.97.1 AWS_ACCESS_KEY_ID=gitcask-dev AWS_SECRET_ACCESS_KEY=gitcask-dev-secret
S3_PORT=${GITCASK_RUSTFS_PORT:-19000}
S="$(dirname "$0")"; W=$(mktemp -d); CACHE=$W/cache; LOG=$W/server.log
PASS=0; FAIL=0
ok()  { echo "  ok   $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL $1"; FAIL=$((FAIL+1)); }
check() { if "$@" >/dev/null 2>&1; then ok "$*"; else bad "$*"; fi; }
s3() { docker run --rm --network host -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY -e AWS_DEFAULT_REGION=us-east-1 amazon/aws-cli:latest --endpoint-url http://127.0.0.1:$S3_PORT s3 "$@" 2>/dev/null; }

cd "$ROOT"
echo "== build =="
cargo build -p gitcask-cli --bin gitcask 2>&1 | grep -E '^(error|warning: unused)' | head -5
BIN=$ROOT/target/debug/gitcask
[ -x "$BIN" ] || { echo "no binary"; exit 1; }

# config: standalone with cache dir and port overridden, auto_create off (the caller creates repos)
sed -e "s|dir = \"/tmp/gitcask\"|dir = \"$CACHE\"|" -e 's|auto_create_on_push = true|auto_create_on_push = false|' -e 's|auth_mode = "jwt"|auth_mode = "none"|' gitcask.standalone.toml > $W/cfg.toml
BASE=http://127.0.0.1:$PORT
start() {
  GITCASK__SERVER__LISTEN=127.0.0.1:$PORT GITCASK__SERVER__PUBLIC_URL="${SERVER_PUBLIC_URL:-$BASE}" "$BIN" serve --config $W/cfg.toml >>$LOG 2>&1 &
  SPID=$!
  for i in $(seq 1 60); do curl -sf $BASE/healthz >/dev/null && return 0; sleep 0.5; done
  echo "server did not start"; tail -20 $LOG; exit 1
}
stop()  { kill $SPID 2>/dev/null; wait $SPID 2>/dev/null; }

REPO=smoke/r1
echo "== phase 1: auth none =="
start
check curl -sf $BASE/readyz
# A conditional create is not retried in the store: while rustfs is down it
# reaches the HTTP boundary as a retryable 503, then succeeds after restart.
RUSTFS_CONTAINER=$(docker compose ps -q rustfs 2>/dev/null)
[ -z "$RUSTFS_CONTAINER" ] && RUSTFS_CONTAINER=$(docker ps -q --filter name=rustfs | head -1)
if [ -z "$RUSTFS_CONTAINER" ]; then
    bad "rustfs container not found"
else
    restore_rustfs() { docker start "$RUSTFS_CONTAINER" >/dev/null 2>&1 || true; }
    trap restore_rustfs EXIT INT TERM
    docker stop "$RUSTFS_CONTAINER" >/dev/null
    outage_code=$(curl --max-time 15 -s -o /dev/null -w '%{http_code}' -X PUT $BASE/smoke/retryable-503.git)
    restore_rustfs
    # Wait for the store to be serving again, not merely accepting connections. The old
    # bound was 5s against a bare `curl /`, which a busy machine loses: the container is
    # back but not ready, and every later case fails for reasons that have nothing to do
    # with the change under test.
    rustfs_back=0
    for _ in $(seq 1 120); do
        health=$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$RUSTFS_CONTAINER" 2>/dev/null)
        if [ "$health" = healthy ]; then rustfs_back=1; break; fi
        if [ "$health" = none ] && curl --max-time 1 -sf -o /dev/null http://127.0.0.1:$S3_PORT/; then rustfs_back=1; break; fi
        sleep 0.5
    done
    [ "$rustfs_back" = 1 ] || echo "  WARN rustfs was not healthy within 60s; later cases may fail spuriously"
    trap - EXIT INT TERM
    recovery_code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT $BASE/smoke/retryable-503.git)
    if [ "$outage_code" = 503 ] && [ "$recovery_code" = 201 ]; then
        ok "store outage create -> 503; recovery -> 201"
    else
        bad "store outage create -> $outage_code; recovery -> $recovery_code"
    fi
    curl -sf -X DELETE $BASE/smoke/retryable-503.git >/dev/null || true
fi
# push to a nonexistent repo must fail (auto_create off)
# -b main: later cases resolve `main` locally; without it the branch name depends on the
# machine's init.defaultBranch (GitHub runners default to master, so 3 merge cases failed).
git init -q -b main $W/src && (cd $W/src && git -c user.name=t -c user.email=t@t commit -q --allow-empty -m one)
if (cd $W/src && git push -q $BASE/$REPO.git HEAD:main 2>/dev/null); then bad "push before create should be refused"; else ok "push before create refused"; fi
check curl -sf -X PUT $BASE/$REPO.git
check sh -c "cd $W/src && git push -q $BASE/$REPO.git HEAD:main"
check git clone -q $BASE/$REPO.git $W/clone1
(cd $W/src && echo x > f && echo old > old && git add f old && git -c user.name=t -c user.email=t@t commit -q -m two && git push -q $BASE/$REPO.git HEAD:main) && ok "second push" || bad "second push"
(cd $W/clone1 && git pull -q && [ -f f ]) && ok "fetch sees second push" || bad "fetch sees second push"
(cd $W/src && FIRST=$(git rev-parse HEAD~1) && curl -sf -H 'Content-Type: application/json' -X PUT -d "{\"target\":\"$FIRST\",\"expected_old_oid\":\"0000000000000000000000000000000000000000\"}" "$BASE/$REPO/api/refs/heads/api/branch" >/dev/null && git ls-remote "$BASE/$REPO.git" refs/heads/api/branch | grep -q "^$FIRST") && ok "write API branch create" || bad "write API branch create"
(cd $W/src && FIRST=$(git rev-parse HEAD~1) && HEAD=$(git rev-parse HEAD) && curl -sf -H 'Content-Type: application/json' -X PUT -d "{\"target\":\"$HEAD\",\"expected_old_oid\":\"$FIRST\"}" "$BASE/$REPO/api/refs/heads/api/branch" >/dev/null && git ls-remote "$BASE/$REPO.git" refs/heads/api/branch | grep -q "^$HEAD") && ok "write API branch CAS move" || bad "write API branch CAS move"
FIRST=$(cd $W/src && git rev-parse HEAD~1); HEAD=$(cd $W/src && git rev-parse HEAD)
code=$(curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' -X PUT -d "{\"target\":\"$FIRST\",\"expected_old_oid\":\"$FIRST\"}" "$BASE/$REPO/api/refs/heads/api/branch"); [ "$code" = 409 ] && ok "write API stale branch CAS -> 409" || bad "write API stale branch CAS -> $code"
(curl -sf -X DELETE "$BASE/$REPO/api/refs/heads/api/branch?expected_old_oid=$HEAD" >/dev/null && [ -z "$(git ls-remote "$BASE/$REPO.git" refs/heads/api/branch)" ]) && ok "write API branch delete" || bad "write API branch delete"
(curl -sf -H 'Content-Type: application/json' -X POST -d '{"name":"api-v1","target":"main","message":"release from API","tagger":{"name":"API Tester","email":"api@example.test","when":"2026-09-01T12:34:56+09:00"}}' "$BASE/$REPO/api/tags" >/dev/null && git ls-remote "$BASE/$REPO.git" | grep -Fq 'refs/tags/api-v1^{}') && ok "write API annotated tag advertises peel" || bad "write API annotated tag advertises peel"
rm -rf $W/api-tag-clone
(git clone -q $BASE/$REPO.git $W/api-tag-clone && git -C $W/api-tag-clone tag -n1 api-v1 | grep -q 'release from API') && ok "annotated tag message survives clone" || bad "annotated tag message survives clone"
mkdir -p $W/archive-tar $W/archive-zip
git -C $W/src archive --format=tar.gz --prefix=snapshot/ HEAD > $W/expected.tar.gz
curl -sf -D $W/archive.headers "$BASE/$REPO/api/archive/main?prefix=snapshot%2F" -o $W/actual.tar.gz
git -C $W/src archive --format=zip --prefix=snapshot/ HEAD > $W/expected.zip
curl -sf "$BASE/$REPO/api/archive/main?format=zip&prefix=snapshot%2F" -o $W/actual.zip
(cmp $W/expected.tar.gz $W/actual.tar.gz && cmp $W/expected.zip $W/actual.zip && tar -xzf $W/actual.tar.gz -C $W/archive-tar && unzip -q $W/actual.zip -d $W/archive-zip && cmp $W/archive-tar/snapshot/f $W/archive-zip/snapshot/f && grep -q '^x$' $W/archive-tar/snapshot/f) && ok "archive tar.gz+zip match git and extract" || bad "archive tar.gz+zip match git and extract"
ETAG=$(awk 'tolower($1) == "etag:" {gsub("\r", "", $2); print $2}' $W/archive.headers | tail -1)
curl -sf -r 0-31 "$BASE/$REPO/api/archive/main?prefix=snapshot%2F" -o $W/archive.range
head -c 32 $W/actual.tar.gz > $W/archive.expected-range
code=$(curl -s -o /dev/null -w '%{http_code}' -H "If-None-Match: $ETAG" "$BASE/$REPO/api/archive/main?prefix=snapshot%2F")
([ -n "$ETAG" ] && cmp $W/archive.range $W/archive.expected-range && [ "$code" = 304 ]) && ok "archive Range + ETag + 304" || bad "archive Range/ETag (etag=$ETAG code=$code)"
PREFIX_CACHE=$(s3 ls "s3://gitcask-test/repos/$REPO/cache/archive/v1/")
[ -z "$PREFIX_CACHE" ] && ok "prefixed archives skip bucket cache" || bad "prefixed archive leaked into bucket cache"
git -C $W/src archive --format=tar.gz HEAD > $W/expected-no-prefix.tar.gz
curl -sf "$BASE/$REPO/api/archive/main" -o $W/actual-no-prefix.tar.gz
ARCHIVE_CACHE_COUNT=$(s3 ls "s3://gitcask-test/repos/$REPO/cache/archive/v1/" | wc -l | tr -d ' ')
(cmp $W/expected-no-prefix.tar.gz $W/actual-no-prefix.tar.gz && [ "$ARCHIVE_CACHE_COUNT" = 1 ]) && ok "prefix-free archive uses bounded bucket cache" || bad "prefix-free archive cache count=$ARCHIVE_CACHE_COUNT"
API_PARENT=$(git -C $W/src rev-parse HEAD)
curl -sf -H 'Content-Type: application/json' -X POST -d "{\"branch\":\"main\",\"message\":\"smoke batch\",\"expected_head_oid\":\"$API_PARENT\",\"committer\":{\"name\":\"Smoke API\",\"email\":\"smoke@gitcask.test\",\"when\":\"2026-09-01T12:34:56+09:00\"},\"changes\":[{\"op\":\"upsert\",\"path\":\"f\",\"content\":\"YXBpCg==\",\"mode\":\"100644\"},{\"op\":\"upsert\",\"path\":\"nested/added\",\"content\":\"YWRkZWQK\",\"mode\":\"100644\"},{\"op\":\"delete\",\"path\":\"old\"}]}" "$BASE/$REPO/api/commits" -o $W/api-commit.json
API_COMMIT=$(sed -n 's/.*"commit_oid":"\([^"]*\)".*/\1/p' $W/api-commit.json)
rm -rf $W/api-commit-clone
(git clone -q $BASE/$REPO.git $W/api-commit-clone && [ "$(cat $W/api-commit-clone/f)" = api ] && [ "$(cat $W/api-commit-clone/nested/added)" = added ] && [ ! -e $W/api-commit-clone/old ] && [ "$(git -C $W/api-commit-clone rev-list --count HEAD)" = 3 ] && [ "$(git -C $W/api-commit-clone rev-parse HEAD)" = "$API_COMMIT" ]) && ok "write API batch commit add+modify+delete" || bad "write API batch commit"
stale_code=$(curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' -X POST -d "{\"branch\":\"main\",\"message\":\"stale\",\"expected_head_oid\":\"$API_PARENT\",\"committer\":{\"name\":\"Smoke API\",\"email\":\"smoke@gitcask.test\",\"when\":\"2026-09-01T12:34:56+09:00\"},\"changes\":[{\"op\":\"upsert\",\"path\":\"stale\",\"content\":\"eAo=\",\"mode\":\"100644\"}]}" "$BASE/$REPO/api/commits")
empty_code=$(curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' -X POST -d "{\"branch\":\"main\",\"message\":\"empty\",\"expected_head_oid\":\"$API_COMMIT\",\"committer\":{\"name\":\"Smoke API\",\"email\":\"smoke@gitcask.test\",\"when\":\"2026-09-01T12:34:56+09:00\"},\"changes\":[{\"op\":\"upsert\",\"path\":\"f\",\"content\":\"YXBpCg==\",\"mode\":\"100644\"}]}" "$BASE/$REPO/api/commits")
([ "$stale_code" = 409 ] && [ "$empty_code" = 400 ]) && ok "write API commit stale 409 + empty 400" || bad "write API commit guards stale=$stale_code empty=$empty_code"
(cd $W/src && ROOT=$(git rev-parse main) && git checkout -q -b smoke-feature $ROOT && echo feature > feature && git add feature && git -c user.name=t -c user.email=t@t commit -q -m feature && git checkout -q -b smoke-base $ROOT && echo base > base && git add base && git -c user.name=t -c user.email=t@t commit -q -m base && git checkout -q -b smoke-conflict-base $ROOT && echo base-side > f && git add f && git -c user.name=t -c user.email=t@t commit -q -m conflict-base && git checkout -q -b smoke-conflict-head $ROOT && echo head-side > f && git add f && git -c user.name=t -c user.email=t@t commit -q -m conflict-head && git push -q $BASE/$REPO.git smoke-feature smoke-base smoke-conflict-base smoke-conflict-head)
SMOKE_BASE=$(git -C $W/src rev-parse smoke-base); SMOKE_CONFLICT_BASE=$(git -C $W/src rev-parse smoke-conflict-base)
curl -sf -H 'Content-Type: application/json' -X POST -d "{\"base\":\"smoke-base\",\"head\":\"smoke-feature\",\"message\":\"smoke merge\",\"committer\":{\"name\":\"Smoke API\",\"email\":\"smoke@gitcask.test\",\"when\":\"2026-09-01T12:34:56+09:00\"},\"strategy\":\"merge\",\"expected_base_oid\":\"$SMOKE_BASE\"}" "$BASE/$REPO/api/merges" -o $W/api-merge.json
API_MERGE=$(sed -n 's/.*"oid":"\([^"]*\)".*/\1/p' $W/api-merge.json)
already_code=$(curl -s -o $W/api-already.json -w '%{http_code}' -H 'Content-Type: application/json' -X POST -d "{\"base\":\"smoke-base\",\"head\":\"smoke-feature\",\"message\":\"again\",\"committer\":{\"name\":\"Smoke API\",\"email\":\"smoke@gitcask.test\",\"when\":\"2026-09-01T12:34:56+09:00\"},\"strategy\":\"merge\",\"expected_base_oid\":\"$API_MERGE\"}" "$BASE/$REPO/api/merges")
([ -n "$API_MERGE" ] && [ "$already_code" = 200 ] && grep -q '"already_merged":true' $W/api-already.json) && ok "write API merge success + already merged" || bad "write API merge/already code=$already_code"
conflict_code=$(curl -s -o $W/api-conflict.json -w '%{http_code}' -H 'Content-Type: application/json' -X POST -d "{\"base\":\"smoke-conflict-base\",\"head\":\"smoke-conflict-head\",\"message\":\"conflict\",\"committer\":{\"name\":\"Smoke API\",\"email\":\"smoke@gitcask.test\",\"when\":\"2026-09-01T12:34:56+09:00\"},\"strategy\":\"merge\",\"expected_base_oid\":\"$SMOKE_CONFLICT_BASE\"}" "$BASE/$REPO/api/merges")
([ "$conflict_code" = 409 ] && grep -q '"conflicts":\["f"\]' $W/api-conflict.json) && ok "write API merge conflict paths -> 409" || bad "write API merge conflict code=$conflict_code body=$(cat $W/api-conflict.json)"
ff_code=$(curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' -X POST -d "{\"base\":\"smoke-base\",\"head\":\"smoke-conflict-head\",\"message\":\"ff only\",\"committer\":{\"name\":\"Smoke API\",\"email\":\"smoke@gitcask.test\",\"when\":\"2026-09-01T12:34:56+09:00\"},\"strategy\":\"fast-forward-only\",\"expected_base_oid\":\"$API_MERGE\"}" "$BASE/$REPO/api/merges")
[ "$ff_code" = 409 ] && ok "write API ff-only divergence -> 409" || bad "write API ff-only divergence -> $ff_code"
(cd $W/src && git tag v1 && git push -q $BASE/$REPO.git v1 && git push -q $BASE/$REPO.git :v1) && ok "tag push+delete" || bad "tag push+delete"
# bucket layout
LS=$(s3 ls --recursive s3://gitcask-test/repos/$REPO/)
echo "$LS" | grep -q manifest.pb && ok "manifest.pb in bucket" || bad "manifest.pb in bucket"
echo "$LS" | grep -q 'wal/.*\.pack' && ok "wal pack in bucket" || bad "wal pack in bucket"
echo "$LS" | grep -q 'log/' && ok "log segment in bucket" || bad "log segment in bucket"
echo "$LS" | sed 's/^/     /' | head -12
# cold re-materialize: stop server, wipe cache, clone again
stop; rm -rf "$CACHE"; start
(git clone -q $BASE/$REPO.git $W/clone2 && [ -f $W/clone2/f ]) && ok "cold clone after cache wipe" || bad "cold clone after cache wipe"
# maintenance ops visible
curl -sf $BASE/$REPO/api/overview | head -c 300; echo
check curl -sf -X DELETE $BASE/$REPO.git
[ -z "$(s3 ls --recursive s3://gitcask-test/repos/$REPO/)" ] && ok "bucket prefix gone after delete" || bad "bucket prefix gone after delete"
stop

echo "== phase 2: auth forwarded =="
sed -i.bak 's|auth_mode = "none"|auth_mode = "forwarded"|' $W/cfg.toml
rm -f $W/cfg.toml.bak
export GITCASK_FORWARD_SECRET=s3cr3t
start
code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT $BASE/$REPO.git); [ "$code" = 401 ] && ok "no headers -> 401" || bad "no headers -> $code"
H=(-H 'X-Gitcask-Principal: user:1' -H 'X-Gitcask-Forward-Secret: s3cr3t')
code=$(curl -s -o /dev/null -w '%{http_code}' "${H[@]}" -X PUT $BASE/$REPO.git); [ "$code" = 403 ] && ok "principal without write -> 403" || bad "principal without write -> $code"
code=$(curl -s -o /dev/null -w '%{http_code}' "${H[@]}" -H 'X-Gitcask-Write: 1' -X PUT $BASE/$REPO.git); [ "$code" = 201 -o "$code" = 200 ] && ok "write header -> create $code" || bad "write header -> $code"
code=$(curl -s -o /dev/null -w '%{http_code}' -H 'X-Gitcask-Principal: user:1' -H 'X-Gitcask-Forward-Secret: wrong' -H 'X-Gitcask-Write: 1' -X PUT $BASE/smoke/r2.git); [ "$code" = 401 ] && ok "wrong secret -> 401" || bad "wrong secret -> $code"
(cd $W/src && git -c http.extraHeader='X-Gitcask-Principal: user:1' -c http.extraHeader='X-Gitcask-Forward-Secret: s3cr3t' -c http.extraHeader='X-Gitcask-Write: 1' push -q $BASE/$REPO.git HEAD:main) && ok "git push with forwarded headers" || bad "git push with forwarded headers"
code=$(curl -s -o /dev/null -w '%{http_code}' "${H[@]}" -H 'Content-Type: application/json' -X PUT -d '{"target":"main"}' "$BASE/$REPO/api/refs/heads/forbidden"); [ "$code" = 403 ] && ok "read-only principal write API refused" || bad "read-only principal write API -> $code"
(cd $W && git -c http.extraHeader='X-Gitcask-Principal: user:2' -c http.extraHeader='X-Gitcask-Forward-Secret: s3cr3t' clone -q $BASE/$REPO.git clone3) && ok "read-only principal can clone" || bad "read-only principal can clone"
(cd $W/clone3 && git -c http.extraHeader='X-Gitcask-Principal: user:2' -c http.extraHeader='X-Gitcask-Forward-Secret: s3cr3t' push -q origin HEAD:main 2>/dev/null) && bad "read-only principal push should fail" || ok "read-only principal push refused"
code=$(curl -s -o /dev/null -w '%{http_code}' "${H[@]}" -H 'X-Gitcask-Write: 1' -X DELETE $BASE/$REPO.git); [ "$code" = 403 ] && ok "delete without admin -> 403" || bad "delete without admin -> $code"
code=$(curl -s -o /dev/null -w '%{http_code}' "${H[@]}" -H 'X-Gitcask-Admin: 1' -X DELETE $BASE/$REPO.git); [ "$code" = 204 -o "$code" = 200 ] && ok "delete with admin -> $code" || bad "delete with admin -> $code"
stop

echo "== phase 3: jwt in gitcask =="
"$BIN" token keygen --private-key "$W/jwt-private.pem" --public-key "$W/jwt-public.pem" >/dev/null
"$BIN" token keygen --private-key "$W/wrong-private.pem" --public-key "$W/wrong-public.pem" >/dev/null
sed -e 's|auth_mode = "forwarded"|auth_mode = "jwt"|' -e "s|public_key = \"gitcask-public.pem\"|public_key = \"$W/jwt-public.pem\"|" -e 's|leeway = "60s"|leeway = "0s"|' "$W/cfg.toml" > "$W/cfg.jwt.toml"
unset GITCASK_FORWARD_SECRET
ADMIN_TOKEN=$("$BIN" --config "$W/cfg.jwt.toml" token mint --key "$W/jwt-private.pem" --principal smoke-admin --scope 'smoke/*:admin' --ttl 1h)
READ_TOKEN=$("$BIN" --config "$W/cfg.jwt.toml" token mint --key "$W/jwt-private.pem" --principal smoke-read --scope 'smoke/*:read' --ttl 1h)
EXPIRED_TOKEN=$("$BIN" --config "$W/cfg.jwt.toml" token mint --key "$W/jwt-private.pem" --principal smoke-expired --scope 'smoke/*:read' --ttl 1s)
WRONG_TOKEN=$("$BIN" --config "$W/cfg.jwt.toml" token mint --key "$W/wrong-private.pem" --principal smoke-wrong --scope 'smoke/*:read' --ttl 1h)
mv "$W/cfg.jwt.toml" "$W/cfg.toml"
start
check curl -sf $BASE/readyz
challenge=$(curl -s -D - -o /dev/null "$BASE/smoke/jwt.git/info/refs?service=git-upload-pack" | tr -d '\r')
echo "$challenge" | grep -q '401 Unauthorized' && echo "$challenge" | grep -qi 'www-authenticate: Basic realm="gitcask"' && ok "jwt no token -> 401 with Basic challenge" || bad "jwt 401 challenge"
ADMIN=(-H "Authorization: Bearer $ADMIN_TOKEN")
READ=(-H "Authorization: Bearer $READ_TOKEN")
code=$(curl -s -o /dev/null -w '%{http_code}' "${ADMIN[@]}" -X PUT $BASE/smoke/jwt.git); [ "$code" = 201 -o "$code" = 200 ] && ok "jwt admin creates repo -> $code" || bad "jwt create -> $code"
AUTH_JWT=http://ignored:$ADMIN_TOKEN@127.0.0.1:$PORT/smoke/jwt.git
(cd $W/src && git push -q $AUTH_JWT HEAD:main) && ok "jwt Basic password push" || bad "jwt Basic password push"
git clone -q $AUTH_JWT $W/jwt-clone && ok "jwt Basic password clone" || bad "jwt Basic password clone"
code=$(curl -s -o /dev/null -w '%{http_code}' "${READ[@]}" "$BASE/smoke/jwt.git/info/refs?service=git-receive-pack"); [ "$code" = 404 ] && ok "jwt read token receive-pack -> 404" || bad "jwt read token receive-pack -> $code"
code=$(curl -s -o /dev/null -w '%{http_code}' "${READ[@]}" -H 'X-Gitcask-Principal: spoofed' -H 'X-Gitcask-Write: 1' -H 'X-Gitcask-Admin: 1' -X DELETE $BASE/smoke/jwt.git); [ "$code" = 404 ] && ok "jwt ignores spoofed forwarded grants -> 404" || bad "jwt spoofed grants -> $code"
code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $WRONG_TOKEN" "$BASE/smoke/jwt.git/info/refs?service=git-upload-pack"); [ "$code" = 401 ] && ok "jwt wrong signature -> 401" || bad "jwt wrong signature -> $code"
sleep 2
expired_headers=$(curl -s -D - -o /dev/null -H "Authorization: Bearer $EXPIRED_TOKEN" "$BASE/smoke/jwt.git/info/refs?service=git-upload-pack" | tr -d '\r')
echo "$expired_headers" | grep -q '401 Unauthorized' && echo "$expired_headers" | grep -qi 'www-authenticate: Basic realm="gitcask"' && ok "jwt expired -> 401 with Basic challenge" || bad "jwt expired challenge"
printf 'LFS through gitcask JWT\n' > $W/lfs-input
if command -v sha256sum >/dev/null 2>&1; then LFS_OID=$(sha256sum $W/lfs-input | awk '{print $1}'); else LFS_OID=$(shasum -a 256 $W/lfs-input | awk '{print $1}'); fi
LFS_SIZE=$(wc -c < $W/lfs-input | tr -d ' ')
LFS_BATCH=$BASE/smoke/jwt.git/info/lfs/objects/batch
upload_batch=$(curl -sf "${ADMIN[@]}" -H 'Content-Type: application/vnd.git-lfs+json' --data "{\"operation\":\"upload\",\"objects\":[{\"oid\":\"$LFS_OID\",\"size\":$LFS_SIZE}]}" $LFS_BATCH)
curl -sf "${ADMIN[@]}" -X PUT --data-binary @$W/lfs-input $BASE/smoke/jwt.git/info/lfs/objects/$LFS_OID >/dev/null
download_batch=$(curl -sf "${READ[@]}" -H 'Content-Type: application/vnd.git-lfs+json' --data "{\"operation\":\"download\",\"objects\":[{\"oid\":\"$LFS_OID\",\"size\":$LFS_SIZE}]}" $LFS_BATCH)
curl -sf "${READ[@]}" $BASE/smoke/jwt.git/info/lfs/objects/$LFS_OID -o $W/lfs-output
echo "$upload_batch" | grep -q '"upload"' && echo "$download_batch" | grep -q '"download"' && cmp -s $W/lfs-input $W/lfs-output && ok "jwt LFS batch + basic upload/download" || bad "jwt LFS batch + basic upload/download"
code=$(curl -s -o /dev/null -w '%{http_code}' "${ADMIN[@]}" -X DELETE $BASE/smoke/jwt.git); [ "$code" = 204 -o "$code" = 200 ] && ok "jwt admin deletes repo -> $code" || bad "jwt delete -> $code"
stop
echo "== result: pass=$PASS fail=$FAIL  (log: $LOG, work: $W) =="
grep -iE 'error|panic' $LOG | grep -v 'refused\|401\|403\|404' | head -10
[ $FAIL -eq 0 ]
