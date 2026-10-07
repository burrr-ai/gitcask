import type { Metadata } from 'next'
import Link from 'next/link'

import { BucketGlyph, Chip, Figure, Flow, ServerGlyph } from '@/lib/components/diagram'
import { DocArticle } from '@/lib/components/docs/doc-article'
import { Section } from '@/lib/components/docs/section'
import { SpecTable } from '@/lib/components/docs/spec-table'
import { DOC_GROUPS, docHref } from '@/lib/content/docs'

export const metadata: Metadata = { title: 'Overview' }

const SECTIONS = [
  { id: 'shape', title: 'The shape' },
  { id: 'requests', title: 'What a request does' },
  { id: 'boundary', title: 'Who owns what' },
  { id: 'pages', title: 'Where to go next' },
]

const PAGE_SUMMARIES: Record<string, string> = {
  quickstart: 'Push to a local gitcask in five minutes.',
  architecture: 'The bucket layout, the write and read paths, and maintenance.',
  'cost-model': 'Round trips per operation and the bars gitcask is held to.',
  authentication: 'JWT, introspection and trusted proxies, and the exact errors.',
  api: 'Git over HTTP, LFS and the JSON API for reads and commits.',
  events: 'Ref events delivered to your webhook from a durable cursor.',
  'initialize-import': 'Start a repository from a pinned tree or a full history.',
  operations: 'Roles, configuration, metrics and recovery.',
  migration: 'Move repositories from Gitea and cut over.',
}

export default function OverviewPage() {
  return (
    <DocArticle
      slug=""
      title="gitcask"
      lede="An open-source git server that keeps every repository in your S3 bucket, so servers are disposable and cost follows pushes rather than repository count."
      sections={SECTIONS}
      sources={['README.md', 'GOAL.md', 'docs/PRODUCT.md']}
    >
      <Section id="shape" title="The shape">
        <p>
          Any number of identical instances serve any repository. Each repository is a write-ahead log in the bucket:
          immutable packs, a log entry per push, and a small manifest that changes only by compare-and-swap. There is
          no database and no leader.
        </p>
        <Figure
          label="Instances with roles serve, serve and maintain, all pointed at one bucket."
          className="not-prose grid gap-3 rounded-card bg-muted p-6 md:p-8"
        >
          <div className="flex flex-wrap gap-2">
            {['serve', 'serve', 'maintain', 'events'].map((role, index) => (
              <Chip key={index}>
                <ServerGlyph className="text-soft-foreground" />
                {role}
              </Chip>
            ))}
          </div>
          <div className="ml-6 h-6 w-px bg-border-strong" aria-hidden="true" />
          <div className="flex items-center gap-3 rounded-control border border-primary bg-primary-surface px-4 py-3 text-primary-ink">
            <BucketGlyph className="size-5" />
            <span className="font-mono text-label font-semibold">repos/&lt;owner&gt;/&lt;repo&gt;/</span>
          </div>
        </Figure>
        <p>
          Roles are configuration, not separate programs: <code>serve</code> answers git, LFS and the API,{' '}
          <code>maintain</code> folds the log into checkpoints and compacts packs, and <code>events</code> delivers
          webhooks. An empty role list runs all three.
        </p>
      </Section>

      <Section id="requests" title="What a request does">
        <p>A read revalidates the manifest first, so every instance answers with the latest push.</p>
        <div className="not-prose">
          <Flow
            steps={[
              { label: 'GET manifest.pb, If-None-Match', code: true },
              { label: '304: serve from cache' },
            ]}
          />
        </div>
        <p>A push is acknowledged only after the bucket accepted it.</p>
        <div className="not-prose">
          <Flow
            steps={[
              { label: 'index the pack' },
              { label: 'PUT pack ∥ idx ∥ log', code: true },
              { label: 'CAS manifest.pb', tone: 'brand', code: true },
              { label: 'ok', tone: 'good' },
            ]}
          />
        </div>
      </Section>

      <Section id="boundary" title="Who owns what">
        <p>
          gitcask owns the bytes. Everything that differs from one organisation to the next belongs to the platform
          built on it.
        </p>
        <SpecTable
          columns={['', 'gitcask', 'Your platform']}
          rows={[
            ['Git', 'Smart HTTP v0 and v2, LFS, the JSON API', 'The UI people use'],
            ['Identity', 'Verifies a signed token and its repository scopes', 'Users, sign-in, teams, revocation'],
            ['Repositories', 'Create, delete, read, commit, merge', 'The list of repositories and their metadata'],
            ['Automation', 'One webhook per ref change, replayable', 'CI, reviews, issues, branch policy'],
          ]}
        />
      </Section>

      <Section id="pages" title="Where to go next">
        <ul className="not-prose grid gap-x-8 gap-y-6 md:grid-cols-2">
          {DOC_GROUPS.flatMap((group) => group.entries)
            .filter((entry) => entry.slug)
            .map((entry) => (
              <li key={entry.slug}>
                <Link href={docHref(entry)} className="group block">
                  <span className="text-title-lg text-foreground group-hover:text-primary">{entry.label}</span>
                  <span className="mt-1 block text-body text-soft-foreground">{PAGE_SUMMARIES[entry.slug]}</span>
                </Link>
              </li>
            ))}
        </ul>
      </Section>
    </DocArticle>
  )
}
