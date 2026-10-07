// Writes the agent-facing documentation into public/ before every dev and build run:
//
//   /llms.txt            the index (https://llmstxt.org): one link per document, grouped
//   /llms-full.txt       every document below, concatenated
//   /llms/<name>.md      each canonical Markdown file, verbatim
//
// People get the hand-written pages under /docs; agents get the full text here. The Markdown in the
// repository stays the only home of each fact (AGENTS.md D54). Output is generated, never committed.

import { mkdir, readFile, rm, writeFile } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const SITE = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const REPOSITORY = path.resolve(SITE, '..')
const PUBLIC = path.join(SITE, 'public')
const REPOSITORY_URL = 'https://github.com/burrr-ai/gitcask'

/** `docs/reference/` is left out: it excerpts Cursor's post, which is theirs to publish. */
const GROUPS = [
  {
    title: 'Start',
    docs: [
      ['README.md', 'readme', 'What gitcask is, a five-minute local run, how it works, running it in production'],
      ['GOAL.md', 'goal', 'The target, the acceptance table, and what gitcask does not optimise for'],
      ['docs/PRODUCT.md', 'product', 'Core versus platform, open source versus cloud, and what is out of scope'],
    ],
  },
  {
    title: 'Design',
    docs: [
      ['AGENTS.md', 'architecture', 'Constraints, the WAL, principles and every design decision in force'],
      ['docs/ROUNDTRIPS.md', 'roundtrips', 'Round trips as the cost model, with the per-operation budgets'],
      ['docs/CONTRACT.md', 'contract', 'The cross-crate contract'],
      ['docs/DIRECTION.md', 'direction', 'How this fork differs from walgit, and the operating decisions'],
    ],
  },
  {
    title: 'Reference',
    docs: [
      ['SECURITY.md', 'security', 'Vulnerability reporting and the mixed direct and trusted-proxy authentication contract'],
      ['docs/INITIALIZE.md', 'initialize', 'Initializing a pristine repository from a pinned tree'],
      ['docs/IMPORT.md', 'import', 'Full-history pristine import: pinning, receipts and bounds'],
      ['docs/EVENTS.md', 'events', 'WAL-derived ref events and the webhook bridge'],
      ['docs/LFS.md', 'lfs', 'LFS objects in the store'],
      ['docs/INTEGRITY.md', 'integrity', 'The object-integrity invariant and the fsck audit'],
    ],
  },
  {
    title: 'Operate',
    docs: [
      ['docs/OPERATIONS.md', 'operations', 'Metrics, symptom-first diagnosis, capacity, incidents and recovery'],
      ['docs/MIGRATION.md', 'migration', 'Migrating repositories from Gitea'],
      ['gitcask.example.toml', 'config', 'Every configuration key with its default and a comment', 'toml'],
    ],
  },
  {
    title: 'Optional',
    docs: [
      ['docs/RELEASING.md', 'releasing', 'How releases are prepared and published'],
      ['CONTRIBUTING.md', 'contributing', 'Development, tests and DCO requirements'],
    ],
  },
]

const SUMMARY =
  'A stateless git server that keeps every repository as a write-ahead log in S3-compatible object storage. ' +
  'No database and no leader; any instance serves any repository, and cost scales with pushes rather than with ' +
  'the number of repositories.'

const NAMES = new Map(GROUPS.flatMap((group) => group.docs.map(([source, name]) => [source, name])))

/**
 * Relative links in the Markdown point at repository files. A file published here links to its
 * /llms copy so an agent stays inside this hierarchy; anything else becomes a GitHub URL.
 */
function rewriteLinks(markdown, source) {
  return markdown.replace(/\]\((?!https?:|mailto:|#)([^)\s]+)\)/g, (_, target) => {
    const [file, hash] = target.split('#')
    const resolved = path.posix.normalize(path.posix.join(path.posix.dirname(source), file))
    const anchor = hash ? `#${hash}` : ''
    const name = NAMES.get(resolved)
    if (name) return `](/llms/${name}.md${anchor})`
    const kind = path.posix.extname(resolved) ? 'blob' : 'tree'
    return `](${REPOSITORY_URL}/${kind}/main/${resolved}${anchor})`
  })
}

async function main() {
  const out = path.join(PUBLIC, 'llms')
  await rm(out, { recursive: true, force: true })
  await mkdir(out, { recursive: true })

  const index = [`# gitcask`, '', `> ${SUMMARY}`, '']
  index.push(
    'Each link below is the canonical document, verbatim. `/llms-full.txt` has all of them in one file.',
    `Source repository: ${REPOSITORY_URL}`,
    ''
  )
  const full = [`# gitcask`, '', `> ${SUMMARY}`, '']

  for (const group of GROUPS) {
    index.push(`## ${group.title}`, '')
    for (const [source, name, description, fence] of group.docs) {
      const raw = await readFile(path.join(REPOSITORY, source), 'utf8')
      const body = fence ? `# ${source}\n\n\`\`\`${fence}\n${raw.trimEnd()}\n\`\`\`\n` : rewriteLinks(raw, source)
      await writeFile(path.join(out, `${name}.md`), body)
      index.push(`- [${source}](/llms/${name}.md): ${description}`)
      full.push(`<!-- ${source} -->`, '', body.trimEnd(), '')
    }
    index.push('')
  }

  await writeFile(path.join(PUBLIC, 'llms.txt'), index.join('\n'))
  await writeFile(path.join(PUBLIC, 'llms-full.txt'), full.join('\n'))
}

await main()
