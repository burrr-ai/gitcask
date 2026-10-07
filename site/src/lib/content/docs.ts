/**
 * The documentation pages and their sidebar order. Each page is hand-written in
 * `src/app/docs/<slug>/page.tsx`; the canonical facts stay in the repository's Markdown, which every
 * page links under "Sources" (AGENTS.md D54).
 */

export const REPOSITORY_URL = 'https://github.com/burrr-ai/gitcask'

export type DocEntry = {
  /** URL segment under /docs; empty for the overview. */
  slug: string
  label: string
}

export type DocGroup = {
  title: string
  entries: DocEntry[]
}

export const DOC_GROUPS: DocGroup[] = [
  {
    title: 'Start',
    entries: [
      { slug: '', label: 'Overview' },
      { slug: 'quickstart', label: 'Quickstart' },
    ],
  },
  {
    title: 'Concepts',
    entries: [
      { slug: 'architecture', label: 'Architecture' },
      { slug: 'cost-model', label: 'Cost model' },
    ],
  },
  {
    title: 'Reference',
    entries: [
      { slug: 'authentication', label: 'Authentication' },
      { slug: 'api', label: 'API' },
      { slug: 'events', label: 'Events' },
      { slug: 'initialize-import', label: 'Initialize and import' },
    ],
  },
  {
    title: 'Operate',
    entries: [
      { slug: 'operations', label: 'Operations' },
      { slug: 'migration', label: 'Migrating from Gitea' },
    ],
  },
]

export const DOCS: DocEntry[] = DOC_GROUPS.flatMap((group) => group.entries)

export function docHref(entry: DocEntry) {
  return entry.slug ? `/docs/${entry.slug}` : '/docs'
}

export function findDoc(slug: string) {
  return DOCS.find((entry) => entry.slug === slug)
}

/** A repository file on GitHub, for "Sources" links and anything the site does not restate. */
export function sourceUrl(path: string) {
  return `${REPOSITORY_URL}/blob/main/${path}`
}
