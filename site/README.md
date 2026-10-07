# gitcask site

The documentation site: a landing page that shows what gitcask is, and every Markdown document in the
repository rendered as a page. It is a separate Next.js app that is never built into or served by gitcask.

- **Content stays where it is.** Pages under `/docs` are `README.md`, `GOAL.md`, `SECURITY.md`,
  `CONTRIBUTING.md`, `AGENTS.md` and `docs/*.md`, read at build time. Edit those files, not the site.
  `src/lib/content/docs.ts` only decides the sidebar order and URL of each file. `docs/reference/` is
  deliberately left out.
- **Design rules** come from the comwit UI template: tokens in `src/app/comwit-tokens.css` (do not edit),
  the gitcask theme in `src/app/globals.css`, components in `src/lib/components/ui`, and the rules in
  [`design.md`](design.md). Read `design.md` before changing a screen.
- **Hosting** is Cloudflare Workers through [OpenNext](https://opennext.js.org/cloudflare). Every page is
  prerendered and served from the static-assets incremental cache, so the Worker never reads Markdown.
  **That cache must be populated on deploy**: use `opennextjs-cloudflare deploy` (what `pnpm run deploy`
  runs), or run `opennextjs-cloudflare populateCache remote` before a bare `wrangler deploy`. Without it
  the Worker falls back to rendering, finds no Markdown, and answers 500 on `/` and `/docs` and 404 on
  every other page.

```sh
pnpm install
pnpm run dev        # http://localhost:3000
pnpm run build      # Next.js production build
pnpm run preview    # OpenNext build + local Worker (wrangler dev)
pnpm run deploy     # OpenNext build + deploy (needs Cloudflare credentials)
```
