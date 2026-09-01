# Git at any scale — the source design

The design gitcask follows is described in Cursor's blog post **"Git at any scale"** by Vicent Martí
(2026-08-18), which introduces the system they call **Continuity**:

> https://cursor.com/blog/git-at-any-scale

The full text was previously mirrored here; it is Cursor's copyright, so this file now keeps only the
pointer and the two paragraphs that define the architecture gitcask implements (quoted under fair use):

> Continuity's insight: make a write-ahead log in object storage the source of truth, and make every
> on-disk repository a cache. A push is stored as an immutable object and becomes visible only when a
> tiny manifest is rewritten with a compare-and-swap. That CAS is the consensus — no election, no
> quorum, no primary.

> This scalability constraint also applies the other way. When agents work with Git repositories at
> scale, they often operate outside of a monorepo by creating vast numbers of small repositories […]
> because the system scales in both directions, every repository gets just the right number of replicas.

Read the post before touching WAL/publish/sync. What gitcask changed relative to walgit's
implementation of this design — and why — is in `docs/DIRECTION.md`.
