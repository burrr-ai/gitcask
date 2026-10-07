/**
 * The five-minute local run, restated from README.md "Try it in five minutes". Shown on the landing
 * page and on /docs/quickstart; change it together with the README.
 */
export const QUICKSTART = {
  start: 'docker compose up --build --wait',
  mint: `TOKEN=$(docker compose run --rm token --config /etc/gitcask/gitcask.standalone.toml token mint \\
  --key /run/secrets/gitcask-private.pem --principal local-demo --scope local/demo:admin --ttl 1h)`,
  create: 'curl -fsS -X PUT -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8080/local/demo',
  push: `git clone "http://ignored:$TOKEN@127.0.0.1:8080/local/demo.git"
cd demo && git commit --allow-empty -m first && git push -u origin HEAD:main`,
}

export const QUICKSTART_SCRIPT = [QUICKSTART.start, QUICKSTART.mint, QUICKSTART.create, QUICKSTART.push].join('\n')
