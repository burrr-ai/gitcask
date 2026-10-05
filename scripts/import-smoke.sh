#!/usr/bin/env bash
# Standalone HTTP contract rig, entirely synthetic. BASE points to a local
# Gitcask. Optional trusted-forwarding secret is supplied by the operator.
set -euo pipefail
base=${1:?Usage: scripts/import-smoke.sh http://127.0.0.1:PORT}
case "$base" in http://127.0.0.1:*|http://localhost:*) ;; *) echo 'Use a disposable local rig only' >&2; exit 2;; esac
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export GIT_CONFIG_GLOBAL="$work/gitconfig" GIT_CONFIG_SYSTEM=/dev/null GIT_TERMINAL_PROMPT=0
ready=0
for _ in $(seq 1 50); do
  if curl --max-time 1 -fsS "$base/healthz" >/dev/null 2>&1; then ready=1; break; fi
  sleep 0.1
done
[[ $ready == 1 ]] || { echo 'Local Gitcask rig is not ready' >&2; exit 1; }
owner="import-smoke-$(date +%s)-$$"
headers=(-H 'X-Gitcask-Principal: synthetic-import-smoke' -H 'X-Gitcask-Write: 1' -H 'X-Gitcask-Admin: 1')
githeaders=(-c 'http.extraHeader=X-Gitcask-Principal: synthetic-import-smoke' -c 'http.extraHeader=X-Gitcask-Write: 1')
if [[ -n ${GITCASK_FORWARD_SECRET:-} ]]; then
  headers+=(-H "X-Gitcask-Forward-Secret: $GITCASK_FORWARD_SECRET")
  githeaders+=(-c "http.extraHeader=X-Gitcask-Forward-Secret: $GITCASK_FORWARD_SECRET")
fi
curl -fsS "${headers[@]}" -X PUT "$base/$owner/source" >/dev/null
curl -fsS "${headers[@]}" -X PUT "$base/$owner/target" >/dev/null
git init -q -b main "$work/source"
git -C "$work/source" -c user.name=Synthetic -c user.email=synthetic@example.test commit -q --allow-empty -m first
git -C "$work/source" branch develop
git -C "$work/source" -c user.name=Synthetic -c user.email=synthetic@example.test tag -a v1 -m annotated
printf 'synthetic history\n' > "$work/source/file"
git -C "$work/source" add file
git -C "$work/source" -c user.name=Synthetic -c user.email=synthetic@example.test commit -q -m second
git "${githeaders[@]}" -C "$work/source" push -q "$base/$owner/source.git" --all
git "${githeaders[@]}" -C "$work/source" push -q "$base/$owner/source.git" --tags
curl -fsS "${headers[@]}" -H 'Content-Type: application/json' -d "{\"source\":\"$owner/source\"}" "$base/$owner/target/api/import/resolve" > "$work/snapshot.json"
python3 - "$work" <<'PY'
import json,sys,pathlib
p=pathlib.Path(sys.argv[1]);s=json.loads((p/'snapshot.json').read_text());(p/'request.json').write_text(json.dumps({'operation_key':'synthetic-smoke','snapshot':s}))
PY
code=$(curl -sS "${headers[@]}" -H 'Content-Type: application/json' --data-binary "@$work/request.json" -o "$work/result.json" -w '%{http_code}' "$base/$owner/target/api/import")
[[ $code == 201 ]] || { cat "$work/result.json"; exit 1; }
curl -fsS "${headers[@]}" -X DELETE "$base/$owner/source" >/dev/null
git "${githeaders[@]}" clone -q --bare "$base/$owner/target.git" "$work/target"
git -C "$work/target" fsck --full --no-dangling
[[ $(git -C "$work/target" rev-list --count refs/heads/main) == 2 ]]
python3 - "$work" <<'PY'
import json,subprocess,sys,pathlib
p=pathlib.Path(sys.argv[1]);s=json.loads((p/'snapshot.json').read_text())
for ref in s['refs']:
 assert subprocess.check_output(['git','-C',str(p/'target'),'rev-parse',ref['name']],text=True).strip()==ref['oid']
PY
code=$(curl -sS "${headers[@]}" -H 'Content-Type: application/json' --data-binary "@$work/request.json" -o "$work/replay.json" -w '%{http_code}' "$base/$owner/target/api/import")
[[ $code == 200 ]]
python3 - "$work" <<'PY'
import json,sys,pathlib
p=pathlib.Path(sys.argv[1]);a=json.loads((p/'result.json').read_text());b=json.loads((p/'replay.json').read_text());assert b['replayed'] and a['seq']==b['seq'] and a['request_hash']==b['request_hash']
PY
hash=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["request_hash"])' "$work/result.json")
curl -fsS "${headers[@]}" "$base/$owner/target/api/import/receipt?operation_key=synthetic-smoke&request_hash=$hash" >/dev/null
curl -fsS "${headers[@]}" -X DELETE "$base/$owner/target" >/dev/null
printf 'import HTTP smoke passed: history, heads/tags, independent clone, replay, receipt\n'
