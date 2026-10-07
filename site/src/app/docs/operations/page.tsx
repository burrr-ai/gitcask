import type { Metadata } from 'next'

import { Callout } from '@/lib/components/docs/callout'
import { CodeBlock } from '@/lib/components/docs/code-block'
import { Definitions } from '@/lib/components/docs/definitions'
import { DocArticle } from '@/lib/components/docs/doc-article'
import { Section, SubSection } from '@/lib/components/docs/section'
import { SpecTable } from '@/lib/components/docs/spec-table'
import { sourceUrl } from '@/lib/content/docs'

export const metadata: Metadata = { title: 'Operations' }

const SECTIONS = [
  { id: 'run', title: 'Run it' },
  { id: 'first-look', title: 'First look' },
  { id: 'metrics', title: 'Metrics and alerts' },
  { id: 'symptoms', title: 'From symptom to cause' },
  { id: 'capacity', title: 'Capacity' },
  { id: 'recovery', title: 'Deploys and recovery' },
]

const DOCKER_RUN = `docker pull ghcr.io/burrr-ai/gitcask:0.0.7
docker run --rm -p 8080:8080 \\
  -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY \\
  -v "$PWD/gitcask.toml:/etc/gitcask/gitcask.toml:ro" \\
  ghcr.io/burrr-ai/gitcask:0.0.7`

const STORE_TOML = `[store]
bucket = "your-bucket"

[store.s3]
credentials = "default"
region = "us-east-1"
endpoint = ""                 # use AWS S3 rather than the local rustfs default
force_path_style = false`

const FIRST_LOOK = `curl -si "$BASE/healthz"
curl -si "$BASE/readyz"
curl -s "$BASE/metrics"

gitcask --config gitcask.toml wal pending
gitcask --config gitcask.toml repo info owner/repo
curl -s "$BASE/owner/repo/api/overview"
curl -s "$BASE/owner/repo/api/tasks"`

const REWIND = `gitcask --config gitcask.toml wal ls owner/repo
gitcask --config gitcask.toml wal show owner/repo 42
gitcask --config gitcask.toml wal materialize owner/repo --at-seq 41 --out /tmp/repo-restore
git -C /tmp/repo-restore fsck --full
git -C /tmp/repo-restore push --force "$REPO_URL" \\
  <old_oid>:refs/heads/<branch>`

const metric = (name: string) => <code key={name}>{name}</code>

export default function OperationsPage() {
  return (
    <DocArticle
      slug="operations"
      title="Operations"
      lede="Run the published image against your bucket, watch a handful of metrics, and recover by starting new instances."
      sections={SECTIONS}
      sources={['docs/OPERATIONS.md', 'README.md', 'gitcask.example.toml', 'AGENTS.md']}
    >
      <Section id="run" title="Run it">
        <p>Images are published for linux/amd64 and linux/arm64. Pin the patch tag; there is no latest tag.</p>
        <CodeBlock code={DOCKER_RUN} />
        <p>
          S3 keys are read from the environment at startup by default. Set <code>credentials = &quot;default&quot;</code>{' '}
          to use the refreshing AWS SDK chain: environment, profile, ECS task role, then instance metadata.
        </p>
        <CodeBlock lang="toml" title="gitcask.toml" code={STORE_TOML} />
        <Definitions
          code
          items={[
            { term: 'serve', children: 'Git, the JSON API and LFS.' },
            { term: 'maintain', children: 'Checkpoints, compaction and fsck, driven by pending markers.' },
            { term: 'events', children: 'The webhook bridge.' },
            {
              term: 'roles = []',
              children: 'All three. Any number of hosts can share one bucket.',
            },
          ]}
        />
        <Callout title="The cache is disposable">
          <p>
            The image keeps its cache at <code>/var/lib/gitcask</code> and runs as UID 1000; a mounted cache directory
            must be writable by that user. Never restore it from backup.
          </p>
        </Callout>
      </Section>

      <Section id="first-look" title="First look">
        <p>
          <code>/healthz</code> and <code>/readyz</code> never touch S3. A 200 does not mean the bucket is fine.
        </p>
        <CodeBlock code={FIRST_LOOK} />
        <Definitions
          code
          items={[
            { term: 'wal pending', children: 'LISTs pending/. A manual check, never on a request path.' },
            {
              term: 'repo info',
              children: 'Runs a full sync and pulls packs local. Read api/overview first.',
            },
            { term: 'api/tasks', children: 'Per instance. Keep its hostname with the log request_id.' },
          ]}
        />
      </Section>

      <Section id="metrics" title="Metrics and alerts">
        <p>Starting points; tune durations to your baseline. Sum counters across instances of a role.</p>
        <SpecTable
          columns={['Metric', 'Normal', 'Alert on']}
          rows={[
            [
              metric('gitcask_push_refused_total{reason}'),
              'Flat outside deploys',
              'One connectivity or unpack immediately; body only as a sustained rate; one draining outside a deploy window',
            ],
            [
              metric('gitcask_store_requests_total{op,outcome}'),
              'ok, ordinary not_found / precondition_failed',
              'One retryable_error or error immediately',
            ],
            [metric('gitcask_store_retries_total{op}'), 'Usually flat', 'Still climbing after 5 minutes'],
            [metric('gitcask_publish_local_apply_failed_total'), '0', 'Any increase'],
            [metric('gitcask_pending_marker_put_failures_total'), '0', 'Any increase; check that repository by hand'],
            [
              metric('gitcask_pending_markers'),
              '0 when idle',
              '> 0 for two whole maintenance intervals; page if pinned at the page limit',
            ],
            [
              metric('gitcask_maintainer_heartbeat_timestamp{host}'),
              'Within two intervals of now',
              <code key="e">time() - value &gt; max(2 * maintenance.interval, 5m)</code>,
            ],
            [
              metric('events_bridge_lag_entries{repo}'),
              '0 after catch-up',
              <>
                &gt; 0 for longer than <code>events.sweep_interval</code>
              </>,
            ],
            [
              metric('gitcask_cache_disk_used_fraction'),
              <>
                Below <code>cache.disk_high_watermark</code>
              </>,
              <>
                Above the watermark for ≥ 2 × <code>cache.evict_interval</code>
              </>,
            ],
            [
              metric('gitcask_runtime_stall_total'),
              'Flat',
              'Any increase; urgent when the log line shows inflight > 0',
            ],
            [metric('gitcask_repo_missing_objects{repo}'), '0', '> 0 is a data-integrity incident, immediately'],
          ]}
        />
        <Callout title="No latency metrics">
          <p>
            There is no store-latency or HTTP-duration metric. Derive percentiles from JSON logs:{' '}
            <code>span.name = store.get|store.head|store.put|store.delete</code> with <code>elapsed_ms</code>.
          </p>
        </Callout>
      </Section>

      <Section id="symptoms" title="From symptom to cause">
        <SpecTable
          columns={['Symptom', 'Look at first']}
          rows={[
            [
              'Clone or push is slow',
              <>
                Band-2 <code>local copy is missing packs</code> / <code>local copy ready</code>, then{' '}
                <code>wal.materialize</code>, <code>wal.download_pack</code> and the push&apos;s{' '}
                <code>receive.body</code> span
              </>,
            ],
            [
              'A push gets 503',
              <>
                <code>/readyz</code> first: 503 is drain. Otherwise store retries and{' '}
                <code>{'{"error":"store_unavailable","retryable":true}'}</code>
              </>,
            ],
            [
              'A pushed ref is missing on another instance',
              <>
                Same bucket and prefix, <code>wal.freshness_ttl = &quot;0s&quot;</code>. If both hold, it is a bug:
                keep the evidence
              </>,
            ],
            [
              'The disk is full',
              <>
                <code>df -h</code> on <code>cache.dir</code>, the watermark, and pack-using work in{' '}
                <code>api/tasks</code>
              </>,
            ],
            [
              'Webhooks are not arriving',
              <>
                <code>events_bridge_lag_entries</code>, then <code>events_bridge_gap_total</code> and{' '}
                <code>events_bridge_sweep_found_total</code>
              </>,
            ],
            [
              'A fresh instance is slow',
              'One materialize task, then fast requests, is a normal cold cache',
            ],
          ]}
        />
        <p>
          Each symptom has a step-by-step check in <a href={sourceUrl('docs/OPERATIONS.md')}>docs/OPERATIONS.md</a>.
        </p>
      </Section>

      <Section id="capacity" title="Capacity">
        <p>The default size is 2 vCPU. Scale by adding instances of the same role, not by raising worker counts.</p>
        <SpecTable
          columns={['Signal', 'Action']}
          rows={[
            [
              <>
                Store, disk and locks healthy; latency and <code>gitcask_http_inflight</code> high on several instances
              </>,
              'Add serving instances',
            ],
            [
              <>
                <code>gitcask_pending_markers</code> does not converge and every maintainer&apos;s workers stay busy
              </>,
              'Add maintainers',
            ],
            [
              <>
                Store retries and <code>store.*</code> <code>elapsed_ms</code> worsen as instances are added
              </>,
              'Investigate the store; more serving multiplies S3 requests',
            ],
          ]}
        />
        <Callout title="Disk floor">
          <p>
            The full live pack set of your largest repository plus its working scratch must fit on one instance, below{' '}
            <code>cache.disk_high_watermark</code>.
          </p>
        </Callout>
      </Section>

      <Section id="recovery" title="Deploys and recovery">
        <SubSection title="Draining">
          <p>SIGTERM drains in two phases. Route deploys on /readyz; /healthz stays 200 while the process lives.</p>
          <SpecTable
            columns={['Phase', '/readyz', 'What changes', 'Bound']}
            rows={[
              ['1. Maintenance drain', '200', 'No new maintenance unit; the running unit is interrupted. Serving is normal', '30 s'],
              [
                '2. Serving drain',
                <>
                  503 + <code>Retry-After: 15</code>
                </>,
                'New fetch, push and LFS object work refused; in-flight requests finish',
                <>
                  <code>server.drain_timeout</code> (<code>&quot;20s&quot;</code>), then 2 s more for the load
                  balancer
                </>,
              ],
            ]}
          />
        </SubSection>
        <SubSection title="Incidents">
          <SpecTable
            columns={['Incident', 'Do']}
            rows={[
              [
                'S3 outage',
                'Reduce write load and restore S3. Verify with a real refs read, then a small push read back from another instance',
              ],
              [
                'All instances lost',
                <>
                  Start new instances with the same config and bucket: <code>gitcask-server --config gitcask.toml</code>
                </>,
              ],
              [
                'Bucket corruption or deletion',
                'Stop writes and restore the original keys from S3 versions or replicas. Never hand-craft a manifest',
              ],
              ['A force push moved a ref', 'Do not roll the bucket back; push the old OID as a new ref update'],
            ]}
          />
        </SubSection>
        <SubSection title="Rewind a ref">
          <CodeBlock code={REWIND} />
        </SubSection>
      </Section>
    </DocArticle>
  )
}
