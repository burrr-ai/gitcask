# PRODUCT — what we build and what we refuse to

This document fixes the **boundaries**: what belongs in the gitcask core versus the calling platform, and what
is open source versus cloud-only. Every feature request is judged by the two rules in §2. When the rules do not
produce an answer, the rules are incomplete — make the decision and write it down here. Boundaries kept in
people's heads drift.

| Document | The question it answers |
|---|---|
| `GOAL.md` | What we aim for technically |
| `docs/DIRECTION.md` | What this fork changed relative to walgit; operating decisions D1–D14 |
| **this document** | How far the product goes |

## 1. Position

**A headless git backend.** The storage, transport, read and commit layer of a git platform, used only through
its API. Offered both self-hosted (open source) and as a cloud service. It has no UI, no issues, no pull
requests, no CI, and no identity.

comwit is to gitcask what an application is to Postgres. Supabase turned Postgres into an API; gitcask does
that for git.

### Why this seat is empty

An AI coding platform has to store *N repositories per user × tens of thousands of users*. Today it has three
answers, all bad:

| Alternative | The problem |
|---|---|
| Delegate to users' GitHub accounts (OAuth) | Users must have a GitHub account; ownership and rate limits sit in someone else's hands |
| Self-host Gitea/GitLab | Cost scales with repository count — exactly what broke for comwit |
| Build it yourself | Collapses the moment history, branches or LFS show up |

gitcask has one weapon: **cost scales with pushes, not with the number of repositories** (D40's pending
markers; confirmed in a 50k-repository spike — 118 vs 120 store requests over five idle minutes). The market
where that weapon matters is platform operators with exploding repository counts, and they already have their
own UI and issue tracker. So we sell storage, transport and reads — nothing above them.

For the same reason we do not aim to *replace Gitea*. A hundred-user team is fine on Gitea, and our weapon
means nothing there.

## 2. The two judging rules

### Rule A — core or platform?

> **Same repository state + same request → always the same result: core.
> The answer differs per organisation: platform.**

| Operation | What the answer depends on | |
|---|---|---|
| Commit a file | parent commit + new content | core |
| Create/delete a branch or tag | ref name + oid | core |
| Merge two refs | two trees + their merge base | core |
| Archive | one commit | core |
| "May this PR be merged?" | approvals, CI status, branch protection — **differs per organisation** | platform |
| Issues, reviews, notifications | data that does not live in the repository | platform |
| "May this user see this repository?" | the organisation's permission model | platform |

GitHub's own API splits along this line: `POST /repos/{o}/{r}/merges` (a plain git merge, no PR involved) is
the core side; `PUT /repos/{o}/{r}/pulls/{n}/merge` (approvals, checks, policy) is the platform side.

**Secondary test**: can it be done with git plumbing alone, no working directory? `update-ref`,
`hash-object` + `commit-tree`, `merge-tree` — if those suffice, it is core.

### Rule B — open source or cloud?

> **Everything needed to run your own repositories on your own infrastructure: OSS.
> Everything needed to run someone else's repositories for them: cloud.**

| | OSS core | Cloud |
|---|---|---|
| git transport (clone/push/LFS/v2) | ● | |
| Read API (refs/resolve/tree/blob/commits/commit/compare) | ● | |
| Write API (branches/tags, archive, batch commits, merge) | ● | |
| Repository create/delete | ● | |
| JWT verification, repository scopes, offline token CLI (§6) | ● | |
| Event webhooks, metrics, `/healthz`, size limits | ● | |
| Migration tooling | ● | |
| Multi-tenancy (tenant isolation, bucket placement) | | ● |
| Metering, billing, tenant quotas | | ● |
| Web console, dashboards | | ● |
| Autoscaling, multi-region, managed backups, SLA, on-call | | ● |

The boundary is also a **repository boundary**: `gitcask` (public) / `gitcask-cloud` (private). Cloud code
never lives in this repository — GitLab's `ee/` layout is the cautionary tale: closed logic leaks into the
core and the split becomes impossible later.

## 3. The litmus test

> **comwit must run on the OSS core alone.**

If comwit runs in production without any cloud feature, the OSS is real and adoption can happen. The moment
"we'd need a cloud feature to run comwit" is uttered, **the line was drawn wrong** — move that feature down
into the OSS. Being our own biggest user keeps the OSS honest automatically.

## 4. Out of scope — with the reason attached

A bare "we don't do that" erodes within six months. Each refusal carries its reason.

| Item | Why not |
|---|---|
| **CI / action runners** | Job queues, runner registration, run history, secrets — all state outside the repository, tens of writes per second. S3 CAS (one write per second per object) cannot carry that, so a database walks in, and at that moment gitcask is Gitea. **The event webhook already connects any CI system** — that is our answer. |
| **Issues, PRs, reviews, wikis, releases** | Data that does not live in the repository (Rule A). |
| **Users, orgs, teams, login, SSO, SCIM** | We own no identity (§6). Owning none, the grey zone does not exist. |
| **Repository listing and search** | It would need LISTs — the exact bottleneck this architecture removed (D40). The caller's database already has the authoritative list. **This surprises OSS adopters, so the README explains it explicitly.** |
| **A web UI** | The platform owns the product; gitcask owns the bytes (`GOAL.md`). |
| **PITR / backup tooling / export** | Already covered by what exists: a force-pushed-away ref comes back via `wal ls`'s `old_oid` + `git push -f`; a backup is a bucket copy; an export is `git clone --mirror`. **The answer is documentation, not code.** |

## 5. Layers and how they separate

```
OSS ──── gitcask        the git engine — auth · transport · WAL · read/write API

Closed ─ gitcask-cloud  multi-tenancy · metering · billing · operations
```

The line along which things *can* separate is **"does it read packs (git objects)?"**

| Level | How | Why |
|---|---|---|
| **Process split** | none | Public-key verification and the handlers' own permission checks live in one process, so there is no duplicated path-to-permission table and no trusted header to forge. |
| **Logical split** | roles (`serve` / `maintain` / `events`) | Want pure storage? Point only `serve` hosts at the bucket. The code boundary is `crates/gitcask-server/src/web/`. |
| **Physical split** | static bytes go to the edge | Raw blobs and archives are immutable and servable without packs — `X-Accel-Redirect` (D23). The real split point for a SaaS whose cost is bandwidth. |

Note the intuition that "clone/push is light, the API is heavy" is **backwards**. `upload-pack` walks the
object graph to build a fresh pack and `receive-pack` indexes and connectivity-checks, so both need every pack
local (D41). `refs`/`resolve` touch zero objects — they are already fully diskless via `SyncLevel::Refs`.

## 6. We own no identity

**The only thing we store about a caller is one opaque `principal` string.** We do not know who they are,
their email, or whether they still exist. With no user table there is **nothing to synchronise and therefore
nothing to conflict with the caller's own authentication.**

Gitea demands a Gitea user before a repository can exist, and at that moment the caller enters user-sync hell:
sign-ups, deletions, suspensions and renames must be mirrored in two places, and every mismatch orphans a
repository. **"A git backend with no user synchronisation" is the differentiator.**

Git clients speak HTTPS Basic, so the EdDSA JWT rides in the password slot (the username is ignored); the API
takes the same token as a Bearer header. The platform signs `sub` (an opaque principal), `scopes`, `exp`,
`iat` and `jti` with its own Ed25519 private key; gitcask verifies with only the public key or a cached JWKS
from `[auth.jwt]`. There is no HS256, no issuance endpoint, no callback, no static token list. Self-hosters
and CI use the offline `gitcask token keygen|mint` commands.

The server's entire authentication state is therefore one public key. Revocation is handled by short expiry
and issuer key rotation; gitcask has no user DB, no sessions, no revocation list, no usage history.
Deployments that already have a trusted IdP proxy can choose `forwarded` mode instead.

## 7. License

**Apache-2.0**, switched from MIT on 2026-09-01, before publication. Both are permissive; Apache adds an
explicit patent grant and patent-retaliation termination, which is what enterprise legal teams look for in
infrastructure software — Kubernetes and Neon's storage engine ship the same way. gitcask is a fork of walgit
(MIT); the original copyright and permission notice are preserved in `NOTICE`, as the MIT terms require.
Relicensing a permissive-licensed fork this way is legal and routine; it is copyleft (GPL/AGPL) code that a
fork cannot relicense.

AGPL or BUSL would fence off cloud vendors, but early on the risk of not being adopted dwarfs the risk of
being copied. And the engine alone cannot be sold as a SaaS — multi-tenancy, provisioning, billing and
operations all live on the closed side, which is a natural moat regardless of the license.

## 8. Current gaps (2026-09-01)

| Item | Status |
|---|---|
| git transport, read API, repository CRUD, event webhooks, metrics, size limits | done |
| Write API — branch/tag CRUD, archive | done (T28): reuses the WAL publish path, `expected_old_oid` CAS, immutable archives |
| Write API — batch file commits, merge | done (T32): one request = one commit = one pack; conflicts are 409 + the conflicting paths |
| Authentication | done (T34): EdDSA JWT verified in-process, repository scopes, Basic/Bearer, public key/JWKS, offline token CLI |
| Migration (bulk Gitea → gitcask) + runbook | done (T27): `gitcask migrate gitea`, resumable, LFS included, `docs/MIGRATION.md` |
| Publication readiness (de-comwit framing, one-command compose, SECURITY/DCO) | done (T33); the human checklist at the end of `README.md` remains |

**Parked** — revisit when the SaaS starts:

- **R2 validation**: Cloudflare R2 has free egress, which decides whether a bandwidth-priced business works at
  all. It is S3-compatible, so this is likely an endpoint question rather than a new backend — the one thing
  to verify against a real account is that conditional PUT (our CAS) behaves. Local rustfs proves nothing here.
- **The cloud control plane**: multi-tenancy, metering, billing, console. It observes traffic and storage and
  **puts prices on them — gitcask itself never knows about billing.**
