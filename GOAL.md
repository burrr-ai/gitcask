# GOAL — what gitcask is for

Context: for everyone (humans and agents) working on this repository. Read this before `AGENTS.md`. This page
is **what we want**; `AGENTS.md` is **how we build it**; `docs/DIRECTION.md` records the decisions that shaped
this fork. When a choice is not obviously right, come back here and ask: which option serves this goal better?

## The one sentence

**A share-nothing git backend for platforms with a very large number of small repositories — one per user
project — created and deleted programmatically, with an object store as the *only* source of truth and
disposable serving instances.**

comwit is gitcask's first user and the workload that shaped it; the architecture is not tied to comwit.

## What that means, unpacked

1. **The object store is the only source of truth.** The bucket *is* the repository. Every push is an
   immutable object + one CAS'd manifest write; every instance is a disposable cache that revalidates with one
   conditional GET. No database inside gitcask, no leader, no node identity. Wipe every instance and lose
   nothing but warmth. (Cursor's *Git at any scale* / Continuity is the design we follow —
   `docs/reference/cursor-git-at-any-scale.md`.)
2. **Share-nothing, elastic.** Any host pointed at the bucket can serve any repository, including its object
   work. Coordination is only through object-store primitives (CAS, leases, content-addressed immutable
   objects). Consistency is never "eventual": push acknowledged ⇒ the next request anywhere sees it.
3. **Many small repositories, not one big one.** The workload is tens of thousands of users × ~20 projects
   each, a few MB to a few hundred MB per repository, pushed frequently by an agent during a session and idle
   for days after. Every cost must scale with *pushes*, never with the *number of repositories*: no pass over
   all repositories, no listing of the bucket on any path that runs periodically. Packs always fit on the
   instance; there is no remote-pack path.
4. **gitcask knows nothing about users.** Identity, permissions, the list of repositories and their metadata
   live in the calling platform's database. gitcask verifies short-lived EdDSA JWTs with a public key/JWKS and
   applies the token's repository scopes; it never stores users, sessions, revocations, or a signing key. It
   exposes create/delete for the platform to call.
   There is no login, no token store, no repository listing.
5. **All the features a git host needs, and only those**: smart HTTP v0/v2 (ls-refs, fetch with
   filter/shallow/deepen, receive-pack atomic/delete/tags/push-options/report-status-v2), LFS,
   `<owner>/<repo>` namespaces, ref events to a webhook, a JSON API for browsing and deterministic Git writes,
   tasks/narration so nothing ever waits silently. Not in scope: code review, merge queues, CI, issues,
   branch protection, per-repository policy — those live in the product built on gitcask.
6. **Predictable for the systems that build on it**: stable, immutable, cacheable artefacts (packs,
   sha-addressed API answers); O(1) ref lookups; latency that does not depend on which instance you hit; a
   provenance log you can rewind to any push (`gitcask wal materialize --at-seq`) — the raw material for
   "restore my project to yesterday".
7. **Use the tools; don't reinvent them.** Upstream `git` for upload-pack, index-pack and repack; Rust +
   tokio + axum for the server; the object store as it is (conditional writes, range reads, multipart); a
   plain proxy in front. Anything git can do, git does; gitcask decides only *where the bytes live*.

## How we know we are there (acceptance)

| Claim | Bar |
|---|---|
| Cold instance is useful in seconds | `ls-remote` of any repository < 1 s on a fresh instance |
| Fresh clone / fetch / push | ordinary smart HTTP works with the client's `git`; `scripts/smoke.sh` passes end to end against rustfs |
| Cache is a cache | stop the server, delete `cache.dir`, start it: every repository clones again from the bucket |
| Push | acknowledged only after the bucket ACKs; one CAS per batch; 5 store requests on the happy path |
| Consistency | push then fetch anywhere sees it; concurrent pushers: exactly one winner (the simulation suite) |
| Cost model | a maintainer pass touches only repositories with a pending marker; no periodic LIST of `repos/` anywhere |
| Security | in `jwt` mode every route but `/healthz` and `/readyz` requires a valid EdDSA token and repo routes require scope; `forwarded` remains available; `none` binds loopback only |
| Transient store errors | 5xx / throttling on any store operation is retried with backoff and never surfaces as a failed push on its own |
| Data completeness | every object reachable from an advertised ref is in the pack set; `fsck` reports violations |

## What we deliberately do **not** optimise for

- A single repository larger than the instance's disk. Packs are always fully local; if a repository outgrows
  the host, the answer is a bigger host or a size limit in the calling platform, not a remote-pack path.
- Human-facing hosting features (web UI, sign-in, branch protection, bundle-uri for CI clones). The calling
  platform owns the product; gitcask owns the bytes.
- Forking git or inventing an object format: weird stuff happens *around* git, never inside it.
