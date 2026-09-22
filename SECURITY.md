# Security policy

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's
[private vulnerability reporting form](https://github.com/burrr-ai/comwit-gitcask/security/advisories/new)
so maintainers can investigate without exposing users before a fix is available.

Include the affected commit or version, deployment shape, impact, reproduction steps, and any suggested
mitigation. Remove live credentials, repository contents, and other third-party data from the report. If the
problem is actively being exploited, say so in the title.

Maintainers will:

- acknowledge the report within three business days;
- provide an initial assessment or request missing information within seven calendar days;
- send at least weekly status updates while a confirmed issue remains unresolved; and
- coordinate a fix, release notes, and public disclosure with the reporter when practical.

Timelines for a fix depend on severity and complexity. Please allow a reasonable remediation window before
publishing details.

## Supported versions

gitcask is pre-1.0. Security fixes are made on `main`; old snapshots and unreleased branches are not maintained.

## Mixed direct and trusted-proxy authentication

`server.auth_mode = "introspect_forwarded"` explicitly enables issuer introspection and trusted
forwarded authentication on the **same process, port and routes** (AGENTS D48). Startup requires valid
`[auth.introspect]` configuration, its service secret, and `GITCASK_FORWARD_SECRET`. Both secrets must
be non-empty printable ASCII without whitespace. Secrets are read at startup; restart to rotate them.
The issuer service secret and proxy secret have separate purposes; provision separate values.

For every protected request, selection happens before permissions are checked:

| `X-Gitcask-Forward-Secret` | Authentication and precedence |
|---|---|
| Absent | Introspect the Basic password or Bearer token using the existing bounded client/cache. Ignore all forwarded identity/write/admin headers. |
| Present but empty, malformed, repeated, or wrong | Return 401; never retry with `Authorization`, even if it contains a valid token. |
| Present once and valid | Compare in constant time, then require a non-empty `X-Gitcask-Principal`. Use only forwarded identity/grants; ignore `Authorization`, even if valid, invalid or more privileged. A missing/empty principal returns 401. |

Header values have surrounding HTTP whitespace trimmed before comparison, as in standalone `forwarded`.
Identities and privileges are **never combined**. Introspected repository scopes retain the JWT grammar
and hierarchy (`admin` implies `write` implies `read`); a scope miss returns 404. A trusted forwarded
principal can read, while `X-Gitcask-Write: 1` and `X-Gitcask-Admin: 1` independently grant write and
admin; a missing grant returns 403. Forwarded grants are not repository scopes: the proxy must authorize
the requested repository and operation before supplying them.

Missing/invalid user credentials or invalid proxy credentials return 401 with
`WWW-Authenticate: Basic realm="gitcask"`. Introspection service failures return 503 with `Retry-After: 5`
and no authentication challenge; valid forwarded requests continue working during an issuer outage.
`/healthz` and `/readyz` remain open. Authentication adds no bucket requests.

The authenticating proxy **must strip every client-supplied `X-Gitcask-Principal`, `X-Gitcask-Write`,
`X-Gitcask-Admin` and `X-Gitcask-Forward-Secret` header**, including duplicates, before injecting its own
identity, grants and secret. Never pass a client's secret header through. Protect the proxy-to-gitcask
hop with TLS or a private trusted transport, and keep the shared secret out of client-visible responses
and logs. Possession of this secret is the request's trust boundary; a route, source address or
`X-Gitcask-Capabilities` header does not establish identity.

Standalone `none`, `jwt`, `introspect` and `forwarded` keep their existing contracts (AGENTS §1.3).
In particular, standalone `forwarded` still permits deployments without a configured secret and must
then be reachable only through a trusted proxy. The combined mode never permits that configuration.
