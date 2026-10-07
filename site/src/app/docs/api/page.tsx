import type { Metadata } from 'next'

import { Chip, Figure } from '@/lib/components/diagram'
import { CodeBlock } from '@/lib/components/docs/code-block'
import { Definitions } from '@/lib/components/docs/definitions'
import { DocArticle } from '@/lib/components/docs/doc-article'
import { Section } from '@/lib/components/docs/section'
import { SpecTable } from '@/lib/components/docs/spec-table'

export const metadata: Metadata = { title: 'API' }

const SECTIONS = [
  { id: 'routing', title: 'One prefix per repository' },
  { id: 'git', title: 'Git and LFS' },
  { id: 'reads', title: 'Reads' },
  { id: 'writes', title: 'Writes' },
  { id: 'commit', title: 'Commit without a clone' },
  { id: 'repository', title: 'Repository and tasks' },
]

const COMMIT_REQUEST = `curl -fsS -X POST http://127.0.0.1:8080/acme/web/api/commits \\
  -H "Authorization: Bearer $TOKEN" \\
  -H 'Content-Type: application/json' \\
  -d @- <<'JSON'
{
  "branch": "main",
  "message": "Update the readme",
  "expected_head_oid": "<current oid of main>",
  "committer": { "name": "CI", "email": "ci@example.com", "when": "2026-10-07T09:00:00Z" },
  "changes": [
    { "op": "upsert", "path": "README.md", "content": "IyBXZWIK", "mode": "100644" },
    { "op": "rename", "from": "docs/old.md", "to": "docs/new.md" },
    { "op": "delete", "path": "tmp/scratch.txt" }
  ]
}
JSON`

/** Method + path cell; paths starting with `/api`, `/info` or `/git-` follow `/{owner}/{repo}`. */
function Route({ method, path }: { method: string; path: string }) {
  return (
    <code className="whitespace-nowrap">
      {method} {path}
    </code>
  )
}

export default function ApiPage() {
  return (
    <DocArticle
      slug="api"
      title="API"
      lede="Every gitcask instance serves git, LFS and a JSON API for every repository under that repository's own path."
      sections={SECTIONS}
      sources={['README.md', 'AGENTS.md', 'crates/gitcask-server/src/web/api/commit.rs']}
    >
      <Section id="routing" title="One prefix per repository">
        <p>
          Everything for a repository starts with <code>/{'{owner}'}/{'{repo}'}</code>, so a proxy routes on the first two
          path segments alone.
        </p>
        <Figure
          label="The repository prefix /{owner}/{repo} leads to git smart HTTP, LFS, the api lane and the api-browser lane."
          className="not-prose grid gap-3 rounded-card bg-muted p-6 md:p-8"
        >
          <div>
            <Chip tone="brand" code>
              /{'{owner}'}/{'{repo}'}
            </Chip>
          </div>
          <div className="ml-6 h-6 w-px bg-border-strong" aria-hidden="true" />
          <div className="flex flex-wrap gap-2">
            {['/info/refs', '/git-upload-pack', '/git-receive-pack', '/info/lfs/…', '/api/…', '/api-browser/…'].map(
              (path) => (
                <Chip key={path} code>
                  {path}
                </Chip>
              )
            )}
          </div>
        </Figure>
        <SpecTable
          columns={['Lane', 'Prefix', 'For']}
          rows={[
            ['Direct', <code key="p">/{'{owner}'}/{'{repo}'}/api</code>, 'Servers and CLIs'],
            [
              'Browser',
              <code key="p">/{'{owner}'}/{'{repo}'}/api-browser</code>,
              <>
                Same handlers; CORS only for <code>server.cors_origins</code>
              </>,
            ],
            ['Reference', <code key="p">/docs</code>, <>Scalar UI over <code>/openapi.json</code></>],
            ['Discovery', <code key="p">/api/v1</code>, 'Not tied to a repository'],
          ]}
        />
      </Section>

      <Section id="git" title="Git and LFS">
        <p>Standard git and git-lfs clients work unchanged. Paths are relative to the repository prefix.</p>
        <SpecTable
          columns={['Route', 'Purpose']}
          rows={[
            [<Route key="r" method="GET" path="/info/refs" />, 'Ref advertisement'],
            [<Route key="r" method="POST" path="/git-upload-pack" />, 'Clone and fetch'],
            [<Route key="r" method="POST" path="/git-receive-pack" />, 'Push'],
            [<Route key="r" method="POST" path="/info/lfs/objects/batch" />, 'LFS batch'],
            [<Route key="r" method="GET HEAD PUT" path="/info/lfs/objects/{oid}" />, 'LFS basic transfer'],
            [<Route key="r" method="POST" path="/info/lfs/verify" />, 'LFS verify'],
          ]}
        />
        <SpecTable
          columns={['Surface', 'Supported']}
          rows={[
            ['Protocol', 'Smart HTTP v0 and v2'],
            ['Fetch', 'ls-refs with prefixes, filter, shallow, deepen, sideband-all'],
            ['Push', 'atomic, delete, tags, push options, report-status-v2'],
          ]}
        />
      </Section>

      <Section id="reads" title="Reads">
        <p>Reads need the <code>read</code> scope. Paths are relative to the repository prefix.</p>
        <SpecTable
          columns={['Route', 'Purpose']}
          rows={[
            [<Route key="r" method="GET" path="/api" />, 'Repository summary'],
            [<Route key="r" method="GET" path="/api/refs" />, 'Get the default ref'],
            [
              <Route key="r" method="GET" path="/api/refs/{kind}" />,
              <>
                List <code>branches</code> or <code>tags</code>; query <code>prefix</code>, <code>q</code>,{' '}
                <code>after</code>, <code>n</code>
              </>,
            ],
            [<Route key="r" method="GET" path="/api/resolve" />, 'Resolve the default ref'],
            [<Route key="r" method="GET" path="/api/resolve/{rest}" />, 'Resolve a revision and optional path'],
            [<Route key="r" method="GET" path="/api/tree/{rest}" />, 'Browse a tree'],
            [
              <Route key="r" method="GET" path="/api/blob/{rest}" />,
              <>
                Read a blob; inline up to 2 MiB, <code>raw</code> for <code>text/plain</code>
              </>,
            ],
            [<Route key="r" method="GET" path="/api/commits" />, 'List commits'],
            [<Route key="r" method="GET" path="/api/commit/{sha}" />, 'Get commit details'],
            [<Route key="r" method="GET" path="/api/compare/{base}...{head}" />, 'Compare two revisions'],
            [<Route key="r" method="GET HEAD" path="/api/archive/{archive_ref}" />, 'Download an immutable repository archive'],
          ]}
        />
      </Section>

      <Section id="writes" title="Writes">
        <p>
          Writes need the <code>write</code> scope. A stale <code>expected_old_oid</code>, <code>expected_head_oid</code>{' '}
          or <code>expected_base_oid</code> returns 409.
        </p>
        <SpecTable
          columns={['Route', 'Purpose']}
          rows={[
            [
              <Route key="r" method="PUT" path="/api/refs/heads/{name}" />,
              <>
                Create or move a branch; body <code>target</code>, optional <code>expected_old_oid</code>
              </>,
            ],
            [
              <Route key="r" method="DELETE" path="/api/refs/heads/{name}" />,
              <>
                Delete a branch; optional <code>?expected_old_oid=</code>
              </>,
            ],
            [<Route key="r" method="PUT" path="/api/refs/tags/{name}" />, 'Create or move a lightweight tag'],
            [<Route key="r" method="DELETE" path="/api/refs/tags/{name}" />, 'Delete a tag'],
            [<Route key="r" method="POST" path="/api/tags" />, 'Create an annotated tag'],
            [<Route key="r" method="POST" path="/api/commits" />, 'Commit a batch of file changes'],
            [
              <Route key="r" method="POST" path="/api/merges" />,
              <>
                Merge one revision into a branch: <code>merge</code>, <code>squash</code> or{' '}
                <code>fast-forward-only</code>; <code>expected_base_oid</code> required
              </>,
            ],
            [<Route key="r" method="POST" path="/api/initialize" />, 'Initialize a pristine repository from a pinned Gitcask tree'],
            [<Route key="r" method="POST" path="/api/import/resolve" />, 'Pin a full-history import source'],
            [<Route key="r" method="POST" path="/api/import" />, 'Import pinned complete history into a pristine repository'],
            [<Route key="r" method="GET" path="/api/import/receipt" />, 'Read a committed import receipt'],
          ]}
        />
      </Section>

      <Section id="commit" title="Commit without a clone">
        <p>
          One request writes blobs, trees and a commit, then moves the branch through the WAL. Replace{' '}
          <code>expected_head_oid</code> with the branch&apos;s current oid; <code>content</code> is standard base64.
        </p>
        <CodeBlock code={COMMIT_REQUEST} />
        <SpecTable
          columns={['Field', 'Value']}
          rows={[
            [<code key="f">changes[].op</code>, <><code>upsert</code> (<code>path</code>, <code>content</code>, <code>mode</code>), <code>delete</code> (<code>path</code>), <code>rename</code> (<code>from</code>, <code>to</code>)</>],
            [<code key="f">mode</code>, <><code>100644</code>, <code>100755</code> or <code>120000</code></>],
            [<code key="f">when</code>, <>RFC 3339 with an explicit offset or <code>Z</code></>],
            [<code key="f">author</code>, <>Optional; defaults to <code>committer</code></>],
            [<code key="f">expected_head_oid</code>, 'Optional; omitted means the branch is force-updated'],
            [<code key="f">allow_empty</code>, <>Optional; defaults to <code>false</code></>],
          ]}
        />
        <p>A 201 answer names what moved.</p>
        <Definitions
          code
          items={[
            { term: 'ref', children: <>The full ref name, such as <code>refs/heads/main</code>.</> },
            { term: 'oid, commit_oid', children: 'The new commit.' },
            { term: 'seq', children: 'The WAL sequence that published it.' },
          ]}
        />
      </Section>

      <Section id="repository" title="Repository and tasks">
        <p>
          Long work runs as a task. Send <code>Accept: text/event-stream</code> to follow it live.
        </p>
        <SpecTable
          columns={['Route', 'Purpose']}
          rows={[
            [<Route key="r" method="PUT" path="/{owner}/{repo}" />, 'Create a repository'],
            [<Route key="r" method="DELETE" path="/{owner}/{repo}" />, 'Delete a repository'],
            [<Route key="r" method="GET" path="/api/overview" />, 'Repository operational status'],
            [<Route key="r" method="GET" path="/api/ops" />, 'List repository operations'],
            [<Route key="r" method="POST" path="/api/ops/{op}" />, 'Run an operation as a task; needs write'],
            [<Route key="r" method="GET" path="/api/tasks" />, 'List repository tasks'],
            [<Route key="r" method="GET" path="/api/tasks/{id}" />, 'Get or attach to a task'],
          ]}
        />
      </Section>
    </DocArticle>
  )
}
