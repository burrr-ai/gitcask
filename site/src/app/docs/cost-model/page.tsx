import type { Metadata } from 'next'
import Link from 'next/link'

import { Chip, Figure } from '@/lib/components/diagram'
import { Definitions } from '@/lib/components/docs/definitions'
import { DocArticle } from '@/lib/components/docs/doc-article'
import { Section } from '@/lib/components/docs/section'
import { SpecTable } from '@/lib/components/docs/spec-table'

export const metadata: Metadata = { title: 'Cost model' }

const SECTIONS = [
  { id: 'primitives', title: 'The latency primitives' },
  { id: 'depth', title: 'Depth before count' },
  { id: 'budgets', title: 'Budgets per operation' },
  { id: 'pushes', title: 'Cost follows pushes' },
  { id: 'rules', title: 'Rules of thumb' },
  { id: 'acceptance', title: 'Acceptance bars' },
]

/** The rounds of a healthy push, in order; requests inside one round run in parallel. */
const PUSH_ROUNDS: { label: string; requests: { label: string; brand?: boolean }[] }[] = [
  { label: 'Round 1', requests: [{ label: 'GET manifest.pb' }] },
  { label: 'Round 2', requests: [{ label: 'PUT pack' }, { label: 'PUT idx' }, { label: 'PUT log' }] },
  { label: 'Round 3', requests: [{ label: 'CAS manifest.pb', brand: true }] },
  { label: 'Round 4', requests: [{ label: 'PUT pending/<o>/<r>' }] },
]

export default function CostModelPage() {
  return (
    <DocArticle
      slug="cost-model"
      title="Cost model"
      lede="Every user-visible latency is a sum of sequential bucket requests, so round trips are the performance design."
      sections={SECTIONS}
      sources={['docs/ROUNDTRIPS.md', 'GOAL.md', 'docs/DIRECTION.md', 'docs/PRODUCT.md']}
    >
      <Section id="primitives" title="The latency primitives">
        <p>The bucket is the only durable primitive. Single-object CAS is its only transaction.</p>
        <SpecTable
          columns={['Primitive', 'Cost', 'Notes']}
          rows={[
            ['GET or PUT of a small object', '60–80 ms p50/p99', 'One request is one round trip'],
            [
              <>
                Conditional GET (<code>If-None-Match</code>) answering 304
              </>,
              '15–18 ms',
              'The cheapest "is anything new?"',
            ],
            ['HEAD', '≈ GET', 'Never added on a happy path'],
            ['404', 'Free', 'Probe, don’t list'],
            ['LIST', 'Slow, paged, eventually-ish', 'Never on a hot path'],
            ['CAS overwrite of one object', 'Serialized, ~1 write/s', 'A throughput cap; 412 is the normal contention signal'],
            ['Range read of a big object', '~100 MB/s per connection', 'Stripe for more; bulk bytes on their own pool'],
          ]}
        />
      </Section>

      <Section id="depth" title="Depth before count">
        <p>Two PUTs in parallel cost one round trip. A healthy push is 6 requests in 4 rounds.</p>
        <Figure
          label="A healthy push: round 1 a manifest freshness GET, round 2 the pack, index and log PUTs in parallel, round 3 the manifest CAS, round 4 the pending-marker PUT."
          className="not-prose flex flex-wrap gap-x-8 gap-y-6 rounded-card bg-muted p-6 md:p-8"
        >
          {PUSH_ROUNDS.map((round) => (
            <div key={round.label} className="grid content-start gap-2">
              <span className="text-label text-soft-foreground">{round.label}</span>
              {round.requests.map((request) => (
                <Chip key={request.label} tone={request.brand ? 'brand' : 'plain'} code className="justify-start">
                  {request.label}
                </Chip>
              ))}
            </div>
          ))}
        </Figure>
        <p>
          The pending marker follows the CAS so an uncommitted push cannot wake maintenance. Its failure never changes
          the committed push.
        </p>
      </Section>

      <Section id="budgets" title="Budgets per operation">
        <p>Happy path: sequential depth, then total store requests.</p>
        <SpecTable
          columns={['Operation', 'Depth', 'Requests']}
          rows={[
            [
              <>
                Any read (<code>info/refs</code>, <code>ls-refs</code>, API refs and resolve)
              </>,
              <>
                1 conditional GET (or 0 within <code>wal.freshness_ttl</code>)
              </>,
              '1',
            ],
            [
              'Cold refs sync',
              '1 manifest GET, then 1 round of checkpoint refs ∥ log tail segments',
              'No checkpoint: 1 + tail (2 with one segment); checkpoint: 2 + tail',
            ],
            [
              'Push',
              'Freshness GET → pack PUT ∥ idx PUT ∥ log PUT → manifest CAS → best-effort pending-marker PUT',
              'Request: 6; already-synced publish: 5',
            ],
            [
              'Ref write API',
              'Freshness GET → optional tag pack PUT ∥ idx PUT ∥ log PUT → manifest CAS → pending-marker PUT',
              'Ref-only: 4; annotated tag: 6',
            ],
            [
              'Commit or merge write API',
              'Freshness GET → new-object pack PUT ∥ idx PUT ∥ log PUT → manifest CAS → pending-marker PUT',
              'Commit or merge commit: 6; fast-forward: 4; already merged: 1',
            ],
            ['Checkpoint', 'Conditional GET → refs PUT ∥ checkpoint PUT → manifest CAS', '3 rounds, 4 requests'],
            ['Lease acquire', '1 GET → 1 CAS put (or 1 Create when absent)', '2'],
            [
              'Maintainer pending pass',
              '1 bounded LIST, then each repository keeps its existing unit depth',
              <>
                Existing requests per repository; up to <code>maintenance.workers</code> repositories overlap
              </>,
            ],
            ['Authentication (introspect mode)', '0 on a cache hit; 1 HTTP request to the introspection URL per miss', '0 store requests'],
          ]}
        />
      </Section>

      <Section id="pushes" title="Cost follows pushes">
        <p>
          A push writes a <code>pending/</code> marker and the maintainer lists only those. Nothing lists{' '}
          <code>repos/</code>, so idle cost does not grow with repository count.
        </p>
        <SpecTable
          columns={['Repositories', 'Store requests over 5 idle minutes']}
          rows={[
            ['100', '118'],
            ['1,000', '118'],
            ['50,000', '120'],
          ]}
        />
        <p>
          <Link href="/docs/architecture#maintainer">Architecture</Link> shows the marker&apos;s path through the
          maintainer.
        </p>
      </Section>

      <Section id="rules" title="Rules of thumb">
        <Definitions
          items={[
            {
              term: 'Let the conditional write be the read',
              children: <p>A 412 on Create means it exists; a 412 on Update means someone moved it. Don&apos;t GET first.</p>,
            },
            {
              term: 'Verify on the failure path',
              children: <p>Probes run only after a failed Create or CAS. The happy path never pays for rare cases.</p>,
            },
            {
              term: 'Batch at the CAS',
              children: (
                <p>
                  Group commit (<code>wal.batch_window</code>) turns N concurrent pushes into one log PUT and one CAS.
                </p>
              ),
            },
            {
              term: 'Carry state in the manifest',
              children: <p>Every request fetches it anyway, so a reader never needs a second request to know what to fetch.</p>,
            },
            {
              term: 'Jitter every retry',
              children: <p>A synchronized retry storm serializes itself on a 1 write/s object.</p>,
            },
          ]}
        />
      </Section>

      <Section id="acceptance" title="Acceptance bars">
        <SpecTable
          columns={['Claim', 'Bar']}
          rows={[
            [
              'Cold instance is useful in seconds',
              <>
                <code>ls-remote</code> of any repository &lt; 1 s on a fresh instance
              </>,
            ],
            [
              'Cache is a cache',
              <>
                Stop the server, delete <code>cache.dir</code>, start it: every repository clones again from the bucket
              </>,
            ],
            ['Push', 'Acknowledged only after the bucket ACKs; one CAS per batch'],
            ['Consistency', 'Push then fetch anywhere sees it; concurrent pushers: exactly one winner'],
            [
              'Cost model',
              <>
                A maintainer pass touches only repositories with a pending marker; no periodic LIST of{' '}
                <code>repos/</code> anywhere
              </>,
            ],
            [
              'Transient store errors',
              '5xx or throttling on any store operation is retried with backoff and never surfaces as a failed push on its own',
            ],
          ]}
        />
      </Section>
    </DocArticle>
  )
}
