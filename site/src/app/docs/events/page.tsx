import type { Metadata } from 'next'
import Link from 'next/link'

import { Figure, Flow } from '@/lib/components/diagram'
import { CodeBlock } from '@/lib/components/docs/code-block'
import { Definitions } from '@/lib/components/docs/definitions'
import { DocArticle } from '@/lib/components/docs/doc-article'
import { Section } from '@/lib/components/docs/section'
import { SpecTable } from '@/lib/components/docs/spec-table'
import { Step, Steps } from '@/lib/components/docs/steps'

export const metadata: Metadata = { title: 'Events' }

const SECTIONS = [
  { id: 'flow', title: 'From the log, not the push' },
  { id: 'event', title: 'The ref event' },
  { id: 'delivery', title: 'Delivery' },
  { id: 'guarantees', title: 'Guarantees' },
  { id: 'configure', title: 'Run the bridge' },
  { id: 'consumer', title: 'Consumer checklist' },
]

const REF_EVENT = `{
  "action": "update",
  "ref_type": "branch",
  "ref_name": "refs/heads/main",
  "old": "48a0637…",
  "new": "cb38da1…",
  "pusher": "alice@example.com",
  "correlation_id": "d1f916f7-…",
  "repo": "acme/monorepo",
  "_gitcask": { "schema_version": 1, "seq": "42", "entry_kind": "push", "request_id": "d1f916f7-…" }
}`

const DELIVERY_HEADERS = `Content-Type:        application/json
X-Gitcask-Delivery:  <sha1 hex of the body>
X-Gitcask-Signature: sha256=<hex HMAC-SHA256(body, events.webhook_secret)>`

const BRIDGE_CONFIG = `[server]
roles = ["events"]            # or leave roles empty on a one-box install: every role, bridge included
[events]
webhook_url = "https://hooks.example.com/gitcask"
webhook_secret = "…"          # env: GITCASK__EVENTS__WEBHOOK_SECRET
sweep_interval = "5m"`

export default function EventsPage() {
  return (
    <DocArticle
      slug="events"
      title="Events"
      lede="One small bridge reads each repository's WAL and posts every committed ref change to your webhook."
      sections={SECTIONS}
      sources={['docs/EVENTS.md', 'AGENTS.md']}
    >
      <Section id="flow" title="From the log, not the push">
        <p>No writer contains event code. A down webhook adds lag, never latency to a push.</p>
        <Figure
          label="A writer commits with a manifest compare-and-swap. The bridge, woken by a notification or a sweep, reads the log from its cursor, posts the events to the webhook, then advances the cursor."
          className="not-prose grid gap-3 rounded-card bg-muted p-6 md:p-8"
        >
          <Flow
            steps={[
              { label: 'any writer', tone: 'quiet' },
              { label: 'CAS manifest.pb', code: true },
              { label: 'POST /_events/notify', code: true },
            ]}
          />
          <div className="ml-6 h-6 w-px bg-border-strong" aria-hidden="true" />
          <Flow
            steps={[
              { label: 'log (cursor, head_seq]', code: true },
              { label: 'POST webhook' },
              { label: '2xx', tone: 'good' },
              { label: 'CAS events/cursor.json', tone: 'brand', code: true },
            ]}
          />
        </Figure>
        <p>
          The cursor lives in the bucket at <code>repos/&lt;o&gt;/&lt;r&gt;/events/cursor.json</code> and advances only after
          your webhook answers 2xx.
        </p>
      </Section>

      <Section id="event" title="The ref event">
        <p>One event per ref update in the transaction. Only <code>ref</code> events exist.</p>
        <CodeBlock code={REF_EVENT} lang="json" />
        <Definitions
          code
          items={[
            {
              term: 'action',
              children: (
                <>
                  <code>create</code>, <code>update</code> or <code>delete</code>. Force is not an action; derive it.
                </>
              ),
            },
            {
              term: 'ref_type',
              children: (
                <>
                  <code>branch</code> for <code>refs/heads/</code>, <code>tag</code> for <code>refs/tags/</code>,{' '}
                  <code>&quot;&quot;</code> otherwise.
                </>
              ),
            },
            {
              term: 'old, new',
              children: (
                <>
                  Always the full zero OID on create and delete, never <code>&quot;&quot;</code>.
                </>
              ),
            },
            {
              term: 'pusher',
              children: (
                <>
                  The opaque principal: the JWT <code>sub</code> or the trusted forwarded principal. See{' '}
                  <Link href="/docs/authentication">Authentication</Link>.
                </>
              ),
            },
            {
              term: 'correlation_id',
              children: (
                <>
                  The push&apos;s request id, also in <code>_gitcask.request_id</code>. An incoming{' '}
                  <code>x-request-id</code> is honoured.
                </>
              ),
            },
            {
              term: '_gitcask.seq',
              children: 'The WAL sequence, as a JSON string.',
            },
            {
              term: '_gitcask.entry_kind',
              children: (
                <>
                  <code>push</code> or <code>ref_update</code>; consumers must not care.
                </>
              ),
            },
          ]}
        />
      </Section>

      <Section id="delivery" title="Delivery">
        <p>
          Each catch-up posts one JSON array of events to <code>events.webhook_url</code>.
        </p>
        <CodeBlock code={DELIVERY_HEADERS} lang="text" title="Request headers" />
        <SpecTable
          columns={['Webhook answers', 'Bridge does']}
          rows={[
            ['2xx', 'Advances the cursor'],
            ['Anything else, or no answer in 10 s', 'Keeps the cursor and retries the same range on the next wake-up'],
          ]}
        />
      </Section>

      <Section id="guarantees" title="Guarantees">
        <Definitions
          items={[
            { term: 'At least once', children: 'The cursor advances only after a 2xx, so whole batches can repeat.' },
            {
              term: 'Dedup key',
              children: (
                <>
                  <code>(repo, _gitcask.seq, ref_name)</code>, or <code>X-Gitcask-Delivery</code> per batch.
                </>
              ),
            },
            {
              term: 'Order',
              children: (
                <>
                  By <code>seq</code> within a repository. Nothing across repositories.
                </>
              ),
            },
            {
              term: 'No no-ops',
              children: (
                <>
                  <code>old == new</code> emits nothing; so do HEAD retargets, compactions and checkpoints.
                </>
              ),
            },
            {
              term: 'Gaps',
              children: (
                <>
                  Only when the bridge lags behind log retention. Counted in <code>events_bridge_gap_total</code>,
                  never repaired silently.
                </>
              ),
            },
          ]}
        />
      </Section>

      <Section id="configure" title="Run the bridge">
        <p>Run one instance with the <code>events</code> role.</p>
        <CodeBlock code={BRIDGE_CONFIG} lang="toml" title="gitcask.toml" />
        <SpecTable
          columns={['Wake-up', 'Role']}
          rows={[
            [
              <code key="w">POST /_events/notify</code>,
              <>
                Bucket notification for a <code>…/manifest.pb</code>: S3 <code>Records[]</code>,{' '}
                <code>{'{"key": …}'}</code> or <code>{'{"repo": "o/r"}'}</code>. Authenticated like every route.
              </>,
            ],
            [
              <code key="w">sweep_interval</code>,
              <>
                Backstop and health check, default <code>5m</code>. A sweep that publishes anything raises{' '}
                <code>events_bridge_sweep_found_total</code>.
              </>,
            ],
          ]}
        />
      </Section>

      <Section id="consumer" title="Consumer checklist">
        <Steps>
          <Step n={1} title="Verify the signature">
            <p>
              Check <code>X-Gitcask-Signature</code> with a constant-time compare before parsing, if you set a secret.
            </p>
          </Step>
          <Step n={2} title="Dedup">
            <p>
              On <code>(repo, _gitcask.seq, ref_name)</code>, or on <code>X-Gitcask-Delivery</code> per batch.
            </p>
          </Step>
          <Step n={3} title="Order within a repository">
            <p>
              Sort by <code>_gitcask.seq</code>. Do not assume order across repositories.
            </p>
          </Step>
          <Step n={4} title="Backfill on a gap">
            <p>Read the missed entries from the WAL.</p>
            <CodeBlock code="gitcask wal ls <repo> --from <seq>" />
          </Step>
        </Steps>
      </Section>
    </DocArticle>
  )
}
