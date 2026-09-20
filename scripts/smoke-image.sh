#!/usr/bin/env bash
# Exercise a built image, including the tools and runtime libraries it ships.
# Usage: scripts/smoke-image.sh <image-or-digest> <expected-build-sha>
set -euo pipefail
image=${1:?image}; expected_sha=${2:?expected build SHA}
work=$(mktemp -d)
prefix="gitcask-image-$(basename "$work" | tr '[:upper:].' '[:lower:]-')"
network="$prefix-net"; store="$prefix-store"; server="$prefix-server"
cleanup() {
    status=$?
    trap - EXIT
    if [ "$status" -ne 0 ]; then
        docker logs "$server" 2>&1 || true
        docker logs "$store" 2>&1 || true
    fi
    docker rm -fv "$server" "$store" >/dev/null 2>&1 || true
    docker network rm "$network" >/dev/null 2>&1 || true
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

docker run --rm "$image" --version | grep -F "$expected_sha"
docker run --rm --entrypoint gitcask-server "$image" --version | grep -F "$expected_sha"
mkdir -p "$work/public" "$work/private"
docker run --rm --user 0:0 -v "$work:/keys" "$image" token keygen \
    --public-key /keys/public/key.pem --private-key /keys/private/key.pem
token=$(docker run --rm --user 0:0 -v "$work/private:/keys:ro" \
    -e GITCASK__SERVER__LISTEN=127.0.0.1:8080 \
    -e GITCASK__AUTH__JWT__ISSUER=image-smoke \
    "$image" --config /dev/null token mint --key /keys/key.pem \
    --principal image-smoke --scope smoke/image:admin --ttl 10m)
basic=$(printf 'ignored:%s' "$token" | base64 | tr -d '\n')

docker network create "$network" >/dev/null
docker run -d --name "$store" --network "$network" \
    -e RUSTFS_ACCESS_KEY=gitcask-dev -e RUSTFS_SECRET_KEY=gitcask-dev-secret \
    -e RUSTFS_ADDRESS=0.0.0.0:9000 -e RUSTFS_VOLUMES=/data rustfs/rustfs:latest >/dev/null
for _ in $(seq 1 120); do
    if docker exec "$store" curl -fsS --max-time 1 http://127.0.0.1:9000/minio/health/live >/dev/null 2>&1; then break; fi
    sleep 0.5
done
docker run --rm --network "$network" \
    -e AWS_ACCESS_KEY_ID=gitcask-dev -e AWS_SECRET_ACCESS_KEY=gitcask-dev-secret \
    -e AWS_DEFAULT_REGION=us-east-1 amazon/aws-cli:latest \
    --endpoint-url "http://$store:9000" s3 mb s3://gitcask-image-smoke

start_server() {
    docker run -d --name "$server" --network "$network" \
        -v "$work/public:/run/gitcask-auth:ro" \
        -e AWS_ACCESS_KEY_ID=gitcask-dev -e AWS_SECRET_ACCESS_KEY=gitcask-dev-secret \
        -e GITCASK__SERVER__AUTH_MODE=jwt -e 'GITCASK__SERVER__ROLES=["serve"]' \
        -e GITCASK__AUTH__JWT__ISSUER=image-smoke \
        -e GITCASK__AUTH__JWT__PUBLIC_KEY=/run/gitcask-auth/key.pem \
        -e GITCASK__STORE__BUCKET=gitcask-image-smoke \
        -e "GITCASK__STORE__S3__ENDPOINT=http://$store:9000" \
        -e GITCASK__STORE__S3__FORCE_PATH_STYLE=true \
        "$image" serve --config /dev/null >/dev/null
    for _ in $(seq 1 120); do
        if docker exec "$server" curl -fsS --max-time 1 http://127.0.0.1:8080/readyz >/dev/null 2>&1; then return; fi
        sleep 0.5
    done
    echo 'image did not become ready' >&2
    return 1
}
start_server
docker exec "$server" curl -fsS http://127.0.0.1:8080/healthz |
    python3 -c 'import json,sys; assert json.load(sys.stdin)["version"] == sys.argv[1]' "$expected_sha"

docker exec -i -e "SMOKE_TOKEN=$token" -e "SMOKE_BASIC=$basic" "$server" sh -eu <<'SH'
export GIT_CONFIG_GLOBAL=/tmp/image-gitconfig GIT_CONFIG_SYSTEM=/dev/null GIT_TERMINAL_PROMPT=0
git config --file "$GIT_CONFIG_GLOBAL" http.extraHeader "Authorization: Basic $SMOKE_BASIC"
git config --file "$GIT_CONFIG_GLOBAL" user.name 'Image smoke'
git config --file "$GIT_CONFIG_GLOBAL" user.email 'smoke@gitcask.test'
git config --file "$GIT_CONFIG_GLOBAL" commit.gpgsign false
git lfs install --skip-repo
base=http://127.0.0.1:8080
test "$(curl -s -o /dev/null -w '%{http_code}' "$base/smoke/image/api")" = 401
test "$(curl -fsS -o /dev/null -w '%{http_code}' -X PUT -H "Authorization: Bearer $SMOKE_TOKEN" "$base/smoke/image")" = 201
git init -q -b main /tmp/source
cd /tmp/source
git lfs install --local
git lfs track '*.bin'
printf 'gitcask image smoke\n' > README.md
dd if=/dev/urandom of=payload.bin bs=1024 count=64 2>/dev/null
git add .
git commit -qm 'Image smoke'
git push -q "$base/smoke/image.git" HEAD:main
git clone -q "$base/smoke/image.git" /tmp/clone
cmp README.md /tmp/clone/README.md
cmp payload.bin /tmp/clone/payload.bin
git -C /tmp/clone fsck --full
curl -fsS -H "Authorization: Bearer $SMOKE_TOKEN" "$base/smoke/image/api/blob/main/README.md?raw=1" > /tmp/api-readme
cmp README.md /tmp/api-readme
SH
payload_sha=$(docker exec "$server" sha256sum /tmp/source/payload.bin | cut -d ' ' -f1)

# Remove this instance AND its anonymous cache volume, keeping only the bucket.
docker stop -t 60 "$server" >/dev/null
docker rm -v "$server" >/dev/null
start_server
docker exec -i -e "SMOKE_BASIC=$basic" "$server" sh -eu <<'SH'
export GIT_CONFIG_GLOBAL=/tmp/image-gitconfig GIT_CONFIG_SYSTEM=/dev/null GIT_TERMINAL_PROMPT=0
git config --file "$GIT_CONFIG_GLOBAL" http.extraHeader "Authorization: Basic $SMOKE_BASIC"
git lfs install --skip-repo
git clone -q http://127.0.0.1:8080/smoke/image.git /tmp/recovered
git -C /tmp/recovered fsck --full
test "$(cat /tmp/recovered/README.md)" = 'gitcask image smoke'
SH
test "$(docker exec "$server" sha256sum /tmp/recovered/payload.bin | cut -d ' ' -f1)" = "$payload_sha"
echo "image smoke passed: $image ($expected_sha)"
