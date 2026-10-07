import type { Metadata } from 'next'

import { Figure, Flow } from '@/lib/components/diagram'
import { Callout } from '@/lib/components/docs/callout'
import { CodeBlock } from '@/lib/components/docs/code-block'
import { Definitions } from '@/lib/components/docs/definitions'
import { DocArticle } from '@/lib/components/docs/doc-article'
import { Section, SubSection } from '@/lib/components/docs/section'
import { SpecTable } from '@/lib/components/docs/spec-table'

export const metadata: Metadata = { title: 'Initialize and import' }

const SECTIONS = [
  { id: 'which', title: 'Which one' },
  { id: 'initialize', title: 'Initialize from a pinned tree' },
  { id: 'import', title: 'Import a full history' },
  { id: 'endpoints', title: 'Endpoints and retries' },
  { id: 'boundaries', title: 'Boundaries and bounds' },
  { id: 'errors', title: 'Errors' },
]

const INITIALIZE_REQUEST = `{
  "source": {"owner": "templates", "repo": "starter", "commit_oid": "0123456789012345678901234567890123456789"},
  "branch": "main",
  "operation_key": "project-creation-7",
  "message": "Initial project",
  "committer": {"name": "Project Builder", "email": "builder@example.test", "when": "2026-09-28T00:00:00Z"}
}`

const INITIALIZE_RESPONSE = `{"ref":"refs/heads/main","commit_oid":"<new root oid>","tree_oid":"<source tree oid>","seq":1,"replayed":false}`

const RESOLVE_REQUEST = `{"source":"templates/starter"}`

const RESOLVE_RESPONSE = `{"source":"templates/starter","object_format":"sha1","refs":[{"name":"refs/heads/develop","oid":"<full lower-case oid>","peeled":""}],"head":{"symbolic_target":"refs/heads/develop","oid":"<full lower-case oid>"},"snapshot_hash":"<sha256>"}`

const IMPORT_REQUEST = `{"operation_key":"creation-123","snapshot":{"source":"templates/starter","object_format":"sha1","refs":[{"name":"refs/heads/develop","oid":"<full lower-case oid>","peeled":""}],"head":{"symbolic_target":"refs/heads/develop","oid":"<full lower-case oid>"},"snapshot_hash":"<sha256>"}}`

const IMPORT_RESPONSE = `{"operation_key":"creation-123","request_hash":"<sha256>","snapshot_hash":"<sha256>","seq":1,"refs_count":1,"head":{"symbolic_target":"refs/heads/develop","oid":"<original oid>"},"replayed":false}`

export default function InitializeImportPage() {
  return (
    <DocArticle
      slug="initialize-import"
      title="Initialize and import"
      lede="Start a pristine repository from one pinned tree, or from another repository's full history, committed by the same manifest compare-and-swap as a push."
      sections={SECTIONS}
      sources={['docs/INITIALIZE.md', 'docs/IMPORT.md', 'AGENTS.md']}
    >
      <Section id="which" title="Which one">
        <p>Both write into an existing, pristine destination and copy every object into its own packs.</p>
        <SpecTable
          columns={['', 'Initialize', 'Import']}
          rows={[
            [
              'Creates',
              'One new parentless commit on one branch, with HEAD pointing at it',
              'Every head and tag, and HEAD (symbolic or detached), with their full reachable history',
            ],
            [
              'Source',
              'A full commit OID in another gitcask repository',
              'A gitcask repository, or a public HTTPS Git URL',
            ],
            ['History', 'None: only the source tree is copied', 'All of it, unchanged'],
            ['Commit content', 'Message, author and committer supplied by the caller', 'The source objects as they are'],
          ]}
        />
      </Section>

      <Section id="initialize" title="Initialize from a pinned tree">
        <p>
          The caller pins a full commit OID, not a branch. Its tree is uploaded into the destination&apos;s{' '}
          <code>wal/</code> before the manifest CAS publishes the new commit.
        </p>
        <Figure
          label="Pinned source commit, then its tree closure uploaded to the destination wal, then the manifest compare-and-swap, then 201."
          className="not-prose rounded-card bg-muted p-6 md:p-8"
        >
          <Flow
            steps={[
              { label: 'pinned commit_oid', tone: 'quiet', code: true },
              { label: 'upload tree closure to wal/' },
              { label: 'CAS manifest.pb', tone: 'brand', code: true },
              { label: '201', tone: 'good' },
            ]}
          />
        </Figure>
        <CodeBlock lang="json" title="POST /{owner}/{repo}/api/initialize" code={INITIALIZE_REQUEST} />
        <CodeBlock lang="json" title="201 Created" code={INITIALIZE_RESPONSE} />
      </Section>

      <Section id="import" title="Import a full history">
        <p>Resolve pins the source refs into a snapshot. Your platform persists it, then imports exactly that snapshot.</p>
        <Figure
          label="Resolve, then the platform persists the snapshot, then import, then the manifest compare-and-swap, then 201."
          className="not-prose rounded-card bg-muted p-6 md:p-8"
        >
          <Flow
            steps={[
              { label: 'POST api/import/resolve', code: true },
              { label: 'persist the snapshot', tone: 'quiet' },
              { label: 'POST api/import', code: true },
              { label: 'CAS manifest.pb', tone: 'brand', code: true },
              { label: '201', tone: 'good' },
            ]}
          />
        </Figure>
        <SubSection title="Resolve">
          <CodeBlock lang="json" title="POST /{owner}/{repo}/api/import/resolve" code={RESOLVE_REQUEST} />
          <CodeBlock lang="json" title="200 OK" code={RESOLVE_RESPONSE} />
        </SubSection>
        <SubSection title="Import">
          <CodeBlock lang="json" title="POST /{owner}/{repo}/api/import" code={IMPORT_REQUEST} />
          <CodeBlock lang="json" title="201 Created" code={IMPORT_RESPONSE} />
        </SubSection>
      </Section>

      <Section id="endpoints" title="Endpoints and retries">
        <p>
          Repeat the identical request to retry. An exact replay returns the original result with{' '}
          <code>replayed:true</code> and never opens the source or moves a ref.
        </p>
        <SpecTable
          columns={['Endpoint', 'Body limit', 'Answers']}
          rows={[
            [<code key="p">POST /{'{owner}'}/{'{repo}'}/api/initialize</code>, '64 KiB', '201 first, 200 exact replay'],
            [
              <code key="p">POST /{'{owner}'}/{'{repo}'}/api/import/resolve</code>,
              '4 KiB',
              '200 snapshot; resolve again if lost before persisting',
            ],
            [<code key="p">POST /{'{owner}'}/{'{repo}'}/api/import</code>, '1 MiB', '201 first, 200 exact replay'],
            [
              <code key="p">GET /{'{owner}'}/{'{repo}'}/api/import/receipt?operation_key=…&amp;request_hash=…</code>,
              'No body',
              '200 with replayed:true, 404 absent, 409 key or hash mismatch',
            ],
          ]}
        />
        <Definitions
          items={[
            { term: <code>operation_key</code>, children: '1–128 printable ASCII characters without whitespace.' },
            {
              term: 'Receipt',
              children:
                'One bounded receipt, committed in the same manifest CAS, records the operation key, the request fingerprint and the original result.',
            },
            {
              term: 'Permissions',
              children:
                'Every attempt, replays included, needs destination write and source read. The receipt GET needs only target read.',
            },
            {
              term: 'Pristine',
              children:
                'No committed WAL work, packs, checkpoint or receipt. A repository whose refs were all deleted is not pristine.',
            },
            {
              term: 'Streaming',
              children: 'With Accept: text/event-stream, work streams as a task with progress; the HTTP status is 200 and the terminal packet carries the result or error.',
            },
          ]}
        />
        <Callout tone="warning" title="Upgrade every manifest writer first">
          <p>
            The receipt lives in <code>manifest.pb</code>. Older binaries, maintainers included, drop unknown fields when
            they rewrite it.
          </p>
        </Callout>
      </Section>

      <Section id="boundaries" title="Boundaries and bounds">
        <Definitions
          items={[
            {
              term: 'Object format',
              children: 'SHA-1 and SHA-256 between gitcask repositories; source and target must match (400 otherwise).',
            },
            {
              term: 'Git LFS',
              children:
                'Any recognized LFS pointer is rejected with 422 before publication. Import checks every historical small blob, not only the tip.',
            },
            { term: 'Gitlinks', children: 'Copied unchanged; submodules are never fetched or recursed.' },
            {
              term: 'External import',
              children:
                'Public HTTPS on port 443, SHA-1 only, no credentials, query or fragment. DNS is pinned to public addresses and every redirect is refused.',
            },
          ]}
        />
        <SpecTable
          columns={['[import] key', 'Default', 'Bounds']}
          rows={[
            [<code key="k">max_refs</code>, <code key="v">1024</code>, 'Heads and tags per snapshot'],
            [<code key="k">max_objects</code>, <code key="v">1000000</code>, 'Acquired historical objects'],
            [<code key="k">max_bytes</code>, <code key="v">&quot;1 GiB&quot;</code>, 'External response bytes and output pack; also server.max_push_bytes'],
            [<code key="k">resolve_timeout</code>, <code key="v">&quot;30s&quot;</code>, 'Refs and HEAD acquisition'],
            [<code key="k">timeout</code>, <code key="v">&quot;15m&quot;</code>, 'Bulk acquisition and validation, before the CAS'],
          ]}
        />
      </Section>

      <Section id="errors" title="Errors">
        <p>
          Non-503 errors are plain text. A 503 is retryable JSON with <code>Retry-After</code>: retry the identical
          request.
        </p>
        <SubSection title="Initialize">
          <SpecTable
            columns={['Status', 'Meaning']}
            rows={[
              ['400', 'Invalid request/identity/branch/full commit ID, non-commit source object, or different object formats'],
              ['401', 'Missing or invalid credentials'],
              ['403', 'Write denied by the existing forwarded-identity contract'],
              ['404', 'Missing or unauthorized repository, or unavailable pinned source commit'],
              ['409', 'Destination is not pristine, or a committed initializer has a different operation key or fingerprint'],
              [
                '413',
                <>
                  Request body or configured <code>server.max_push_bytes</code> pack limit exceeded
                </>,
              ],
              ['422', 'Source tree contains a Git LFS pointer; this operation does not copy LFS payloads'],
              ['503', 'Temporary object-store/authentication-service failure; retry the identical request'],
            ]}
          />
        </SubSection>
        <SubSection title="Import">
          <SpecTable
            columns={['Status', 'Meaning']}
            rows={[
              ['400', 'Invalid snapshot/hash/format/source URL or unsafe DNS result'],
              ['401 / 403 / 404', 'Existing auth/permission/missing-repository contract'],
              ['409', 'Nonpristine, operation conflict, or pinned source unavailable'],
              ['413', 'JSON, refs, transfer, output pack or object bound exceeded'],
              ['422', 'Empty/unborn source, LFS pointer history, or source requiring auth/redirect/dumb HTTP'],
              ['503', 'Resolve busy, deadline, source DNS/transport, store/auth failure or drain; retry fixed request'],
            ]}
          />
        </SubSection>
      </Section>
    </DocArticle>
  )
}
