# gitcask site

The gitcask website: a landing page that shows why to use gitcask, hand-written docs pages for people,
and the repository's Markdown for agents. It is a separate Next.js app; gitcask never builds or serves it.

- **Landing** (`src/app/page.tsx`, `src/lib/components/landing/`): one value claim per scene, each with
  an interactive object that proves it. Every number traces to the repository's Markdown.
- **Docs** (`src/app/docs/<slug>/page.tsx`): diagrams, tables and code, not prose. The sidebar order is
  `src/lib/content/docs.ts`. A change to a fact a page shows updates that page in the same change.
- **For agents**: `scripts/llms.mjs` runs before every dev and build and writes `/llms.txt`,
  `/llms-full.txt` and `/llms/<name>.md` into `public/` from the canonical Markdown, verbatim.
  `docs/reference/` is deliberately left out.
- **Design rules** come from the comwit UI template: tokens in `src/app/comwit-tokens.css` (do not
  edit), the gitcask theme in `src/app/globals.css`, components in `src/lib/components/ui`, and the
  rules in [`design.md`](design.md). Read `design.md` before changing a screen.
- **Hosting** is Cloudflare Workers through [OpenNext](https://opennext.js.org/cloudflare). Pages are
  prerendered and served from the static-assets incremental cache; the `llms` files are static assets.
  Populate that cache on deploy: use `opennextjs-cloudflare deploy` (what `pnpm run deploy` runs), or run
  `opennextjs-cloudflare populateCache remote` before a bare `wrangler deploy`. Without it pages still
  render, but on every request, and the Worker logs a read-only cache error each time.

```sh
pnpm install
pnpm run dev        # http://localhost:3000 (from the repository root: pnpm run docs)
pnpm run build      # Next.js production build
pnpm run preview    # OpenNext build + local Worker (wrangler dev)
pnpm run deploy     # OpenNext build + deploy (needs Cloudflare credentials)
```
