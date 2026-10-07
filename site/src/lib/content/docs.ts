/**
 * The documentation is the Markdown already in the repository — one home per fact (AGENTS.md §0).
 * This table only decides where each file appears on the site; the words stay in the source files.
 * `docs/reference/` is deliberately absent: it excerpts Cursor's post, which is theirs to publish.
 */

export const REPOSITORY_URL = 'https://github.com/burrr-ai/gitcask'

export type DocEntry = {
  /** URL segment under /docs; empty for the docs index. */
  slug: string
  /** Sidebar label. The page title is the file's own first heading. */
  label: string
  /** Path from the repository root. */
  source: string
}

export type DocGroup = {
  title: string
  entries: DocEntry[]
}

export const DOC_GROUPS: DocGroup[] = [
  {
    title: 'Start',
    entries: [
      { slug: '', label: 'Introduction', source: 'README.md' },
      { slug: 'goal', label: 'Goal', source: 'GOAL.md' },
      { slug: 'product', label: 'Product boundary', source: 'docs/PRODUCT.md' },
      { slug: 'direction', label: 'Direction', source: 'docs/DIRECTION.md' },
    ],
  },
  {
    title: 'Design',
    entries: [
      { slug: 'architecture', label: 'Architecture', source: 'AGENTS.md' },
      { slug: 'roundtrips', label: 'Round trips', source: 'docs/ROUNDTRIPS.md' },
      { slug: 'contract', label: 'Crate contract', source: 'docs/CONTRACT.md' },
    ],
  },
  {
    title: 'Features',
    entries: [
      { slug: 'initialize', label: 'Initialize', source: 'docs/INITIALIZE.md' },
      { slug: 'import', label: 'Import', source: 'docs/IMPORT.md' },
      { slug: 'events', label: 'Events', source: 'docs/EVENTS.md' },
      { slug: 'lfs', label: 'LFS', source: 'docs/LFS.md' },
      { slug: 'integrity', label: 'Integrity', source: 'docs/INTEGRITY.md' },
    ],
  },
  {
    title: 'Operate',
    entries: [
      { slug: 'operations', label: 'Operations', source: 'docs/OPERATIONS.md' },
      { slug: 'security', label: 'Security', source: 'SECURITY.md' },
      { slug: 'migration', label: 'Migrating from Gitea', source: 'docs/MIGRATION.md' },
      { slug: 'releasing', label: 'Releasing', source: 'docs/RELEASING.md' },
      { slug: 'contributing', label: 'Contributing', source: 'CONTRIBUTING.md' },
    ],
  },
]

export const DOCS: DocEntry[] = DOC_GROUPS.flatMap((group) => group.entries)

export function docHref(entry: DocEntry) {
  return entry.slug ? `/docs/${entry.slug}` : '/docs'
}

export function findDocBySlug(slug: string) {
  return DOCS.find((entry) => entry.slug === slug)
}

export function findDocBySource(source: string) {
  return DOCS.find((entry) => entry.source === source)
}

export function sourceUrl(path: string) {
  return `${REPOSITORY_URL}/blob/main/${path}`
}
