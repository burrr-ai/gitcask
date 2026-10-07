import type { Metadata } from 'next'
import Link from 'next/link'

import { BucketGlyph, Chip, Figure, Flow } from '@/lib/components/diagram'
import { Definitions } from '@/lib/components/docs/definitions'
import { DocArticle } from '@/lib/components/docs/doc-article'
import { Section } from '@/lib/components/docs/section'
import { SpecTable } from '@/lib/components/docs/spec-table'

export const metadata: Metadata = { title: 'Architecture' }

const SECTIONS = [
  { id: 'bucket-layout', title: 'The bucket layout' },
  { id: 'write-path', title: 'Write path' },
  { id: 'read-path', title: 'Read path' },
  { id: 'checkpoints-compaction', title: 'Checkpoints and compaction' },
  { id: 'maintainer', title: 'The maintainer' },
  { id: 'recovery', title: 'Recovery' },
]

const FIGURE = 'not-prose rounded-card bg-muted p-6 md:p-8'

export default function ArchitecturePage() {
  return (
    <DocArticle
      slug="architecture"
      title="Architecture"
      lede="Each repository is a write-ahead log in the bucket; every instance is a cache or a reader of that log."
      sections={SECTIONS}
      sources={['AGENTS.md', 'README.md', 'gitcask.example.toml']}
    >
      <Section id="bucket-layout" title="The bucket layout">
        <p>
          Logs, packs and checkpoints are immutable. Nothing is visible before the manifest&apos;s compare-and-swap.
        </p>
        <Figure
          label="The repository prefix in the bucket holds manifest.pb, the commit point, beside log, wal, checkpoints, leases, cache and lfs."
          className={`${FIGURE} grid gap-4`}
        >
          <span className="flex items-center gap-2 font-mono text-label text-foreground">
            <BucketGlyph className="text-soft-foreground" />
            repos/&lt;owner&gt;/&lt;repo&gt;/
          </span>
          <div className="flex flex-wrap gap-2 border-l border-border-strong pl-4">
            <Chip tone="brand" code>
              manifest.pb
            </Chip>
            {['log/', 'wal/', 'checkpoints/', 'leases/', 'cache/', 'lfs/'].map((name) => (
              <Chip key={name} code>
                {name}
              </Chip>
            ))}
          </div>
        </Figure>
        <Definitions
          code
          items={[
            {
              term: 'manifest.pb',
              children: (
                <p>
                  Tiny, CAS-rewritten: <code>head_seq</code>, the live pack set, log segments, the checkpoint pointer.
                  The linearization point.
                </p>
              ),
            },
            {
              term: 'log/<first_seq>.pb',
              children: <p>Immutable PUSH, REF_UPDATE and COMPACT entries. One small object per publish batch.</p>,
            },
            {
              term: 'wal/<checksum>.pack/.idx/.rev/.bitmap/.commit-graph',
              children: <p>Immutable packs, content-addressed by pack checksum, plus the side-files a reader needs.</p>,
            },
            {
              term: 'checkpoints/<seq>/',
              children: (
                <p>
                  Folded state at <code>seq</code>: live pack set and full <code>RefSnapshot</code>.
                </p>
              ),
            },
            {
              term: 'leases/<name>.pb',
              children: <p>CAS lease with TTL heartbeat. The only cross-instance mutex.</p>,
            },
            {
              term: 'cache/api/v1/, cache/archive/v1/',
              children: <p>Shared cache of immutable API answers and archives.</p>,
            },
            {
              term: 'lfs/objects/<aa>/<bb>/<oid>',
              children: <p>LFS objects, sha256-addressed and immutable.</p>,
            },
          ]}
        />
      </Section>

      <Section id="write-path" title="Write path">
        <p>A push is acknowledged only after the bucket acknowledged it. Concurrent writers race on one CAS.</p>
        <Figure
          label="A push is indexed, its pack, index and log entry are uploaded in parallel, the manifest is swapped, then ok is returned and a pending marker written. On a 412 the writer refetches, revalidates and retries."
          className={`${FIGURE} grid gap-6`}
        >
          <Flow
            steps={[
              { label: 'git push', tone: 'quiet' },
              { label: 'index-pack' },
              { label: 'PUT pack ∥ idx ∥ log', code: true },
              { label: 'CAS manifest.pb', tone: 'brand', code: true },
              { label: 'ok', tone: 'good' },
              { label: 'PUT pending/<o>/<r>', code: true },
            ]}
          />
          <Flow
            steps={[
              { label: '412', code: true },
              { label: 'refetch the manifest' },
              { label: 'revalidate old values' },
              { label: 'retry with jittered backoff' },
            ]}
          />
        </Figure>
        <p>
          A ref that moved meanwhile is answered <code>ng</code>. Concurrent pushes on one instance share one CAS within{' '}
          <code>wal.batch_window</code> (default <code>5ms</code>) per repository.
        </p>
      </Section>

      <Section id="read-path" title="Read path">
        <p>Every read starts with a conditional GET of the manifest, so every instance is as fresh as a fetch.</p>
        <Figure
          label="A conditional GET of manifest.pb answers 304 and the local copy is served, or 200 and new entries are applied first."
          className={`${FIGURE} grid gap-3`}
        >
          <Flow
            steps={[
              { label: 'GET manifest.pb, If-None-Match', code: true },
              { label: '304: serve the local copy', tone: 'good' },
            ]}
          />
          <Flow
            steps={[
              { label: 'GET manifest.pb, If-None-Match', code: true },
              { label: '200: apply new entries, then serve' },
            ]}
          />
        </Figure>
        <SpecTable
          columns={['Sync level', 'Brings', 'Used by']}
          rows={[
            [
              'Refs',
              <>
                Checkpoint <code>RefSnapshot</code> and every log entry&apos;s ref transaction. No packs.
              </>,
              <>
                <code>info/refs</code>, <code>ls-refs</code>, API refs, resolve and overview
              </>,
            ],
            [
              'Full',
              'Refs and every live pack, downloaded locally in parallel stripes',
              'upload-pack, receive-pack, API object endpoints, compaction',
            ],
          ]}
        />
      </Section>

      <Section id="checkpoints-compaction" title="Checkpoints and compaction">
        <p>
          A checkpoint folds the log, so a cold start reads a snapshot and the tail, never the whole log. Compaction
          folds fresh packs geometrically under a lease, and needs at least two of them.
        </p>
        <SpecTable
          columns={['Key', 'Fires when', 'Default']}
          rows={[
            [<code key="k">wal.snapshot_every_entries</code>, 'This many log entries since the last checkpoint', <code key="v">256</code>],
            [<code key="k">wal.checkpoint_interval</code>, 'The last checkpoint is this old', <code key="v">1h</code>],
            [<code key="k">wal.checkpoint_tail_bytes</code>, 'The log tail after it exceeds this', <code key="v">8MiB</code>],
            [
              <code key="k">compaction.trigger_packs</code>,
              'This many tier-0 packs exist',
              <code key="v">16</code>,
            ],
            [
              <code key="k">compaction.trigger_bytes</code>,
              'Tier-0 pack bytes exceed this',
              <code key="v">1GiB</code>,
            ],
            [
              <code key="k">compaction.retention_superseded</code>,
              'Superseded packs, folded logs and old checkpoints are kept this long before bucket GC',
              <code key="v">7d</code>,
            ],
          ]}
        />
      </Section>

      <Section id="maintainer" title="The maintainer">
        <p>
          A push leaves a marker; the maintainer visits only marked repositories and does one bounded unit of the most
          important missing work at a time.
        </p>
        <Figure
          label="A push writes a pending marker, the maintainer lists pending markers, runs units for each marked repository, and deletes the marker conditionally. Units run in priority order: checkpoint, compaction, rev-index, fsck audit, bucket GC."
          className={`${FIGURE} grid gap-6`}
        >
          <Flow
            steps={[
              { label: 'PUT pending/<o>/<r>', code: true },
              { label: 'LIST pending/', tone: 'brand', code: true },
              { label: 'run units' },
              { label: 'conditional DELETE of the marker' },
            ]}
          />
          <Flow
            steps={[
              { label: 'checkpoint' },
              { label: 'compaction' },
              { label: 'rev-index' },
              { label: 'fsck audit' },
              { label: 'bucket GC' },
            ]}
          />
        </Figure>
        <p>
          Everything it produces is a pure function of config and WAL state, so a deleted artefact is rebuilt
          identically. A repository nobody pushes to is never visited; <Link href="/docs/cost-model">Cost model</Link>{' '}
          has the numbers.
        </p>
      </Section>

      <Section id="recovery" title="Recovery">
        <p>Disk and memory are caches. Wipe every instance and you lose only warmth.</p>
        <Figure
          label="Every instance is wiped; a new instance reads the manifest, checkpoint and log tail and serves refs in under a second."
          className={FIGURE}
        >
          <Flow
            steps={[
              { label: 'every instance wiped', tone: 'gone' },
              { label: 'new instance' },
              { label: 'manifest + checkpoint + tail', tone: 'brand' },
              { label: 'refs in < 1 s', tone: 'good' },
            ]}
          />
        </Figure>
        <p>Packs download on the first object request.</p>
      </Section>
    </DocArticle>
  )
}
