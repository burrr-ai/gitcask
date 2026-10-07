import type { Metadata } from 'next'

import { Figure, Flow } from '@/lib/components/diagram'
import { Callout } from '@/lib/components/docs/callout'
import { CodeBlock } from '@/lib/components/docs/code-block'
import { Definitions } from '@/lib/components/docs/definitions'
import { DocArticle } from '@/lib/components/docs/doc-article'
import { Section } from '@/lib/components/docs/section'
import { Step, Steps } from '@/lib/components/docs/steps'

export const metadata: Metadata = { title: 'Migrating from Gitea' }

const SECTIONS = [
  { id: 'what-moves', title: 'What moves' },
  { id: 'prerequisites', title: 'Prerequisites' },
  { id: 'procedure', title: 'Procedure' },
  { id: 'limitations', title: 'What is not migrated' },
]

const PREVIEW = `export GITEA_TOKEN='<token>'
gitcask --config gitcask.toml migrate gitea \\
  --url https://git.example.com \\
  --owner acme \\
  --to-owner acme \\
  --dry-run`

const PREVIEW_SELECTED = `gitcask --config gitcask.toml migrate gitea \\
  --url https://git.example.com \\
  --owner acme \\
  --repo api --repo web \\
  --dry-run`

const RUN = `gitcask --config gitcask.toml migrate gitea \\
  --url https://git.example.com \\
  --owner acme \\
  --to-owner imported-acme \\
  --concurrency 2 \\
  --state /var/lib/gitcask-migration/acme.json`

const VERIFY = `git clone --mirror https://git.example.com/acme/api.git gitea-api.git
git clone --mirror https://gitcask.example.com/imported-acme/api.git gitcask-api.git

git -C gitea-api.git show-ref | sort > /tmp/gitea-api.refs
git -C gitcask-api.git show-ref | sort > /tmp/gitcask-api.refs
diff -u /tmp/gitea-api.refs /tmp/gitcask-api.refs

git -C gitea-api.git rev-list --all --count
git -C gitcask-api.git rev-list --all --count
git -C gitcask-api.git fsck --full`

export default function MigrationPage() {
  return (
    <DocArticle
      slug="migration"
      title="Migrating from Gitea"
      lede="Copy the Git history, branches, tags and LFS objects of every repository of one Gitea owner into gitcask, without touching Gitea."
      sections={SECTIONS}
      sources={['docs/MIGRATION.md']}
    >
      <Section id="what-moves" title="What moves">
        <p>
          The migrator writes straight to the bucket through the same WAL import path as <code>gitcask import</code>. No
          gitcask server needs to be running.
        </p>
        <Figure
          label="Gitea, then a mirror clone on the migration host, then the WAL import into the bucket, then LFS objects."
          className="not-prose rounded-card bg-muted p-6 md:p-8"
        >
          <Flow
            steps={[
              { label: 'Gitea', tone: 'quiet' },
              { label: 'mirror clone' },
              { label: 'WAL import into the bucket', tone: 'brand' },
              { label: 'LFS objects' },
              { label: 'recorded complete', tone: 'good' },
            ]}
          />
        </Figure>
      </Section>

      <Section id="prerequisites" title="Prerequisites">
        <Definitions
          items={[
            {
              term: 'Gitea token',
              children: (
                <>
                  <code>read:repository</code> and <code>read:user</code>, plus <code>read:organization</code> for an
                  organization owner. Test it against private repositories first.
                </>
              ),
            },
            {
              term: 'Tools',
              children: (
                <>
                  <code>git</code> on <code>PATH</code>, and <code>git-lfs</code> when any source repository uses LFS.
                </>
              ),
            },
            { term: 'gitcask config', children: 'A config file with access to the destination S3 bucket.' },
            {
              term: 'Disk',
              children:
                'Room for about the largest repositories processed at once: each worker holds one mirror clone plus its pack cache. Default concurrency is 2.',
            },
            {
              term: 'Network and state',
              children: 'Access to both Gitea and S3, and durable local storage for the state file.',
            },
          ]}
        />
      </Section>

      <Section id="procedure" title="Procedure">
        <Steps>
          <Step n={1} title="Preview">
            <p>Lists destination names and Gitea-reported sizes. Nothing is cloned or written.</p>
            <CodeBlock code={PREVIEW} />
            <p>
              Repeat <code>--repo</code> to select repositories.
            </p>
            <CodeBlock code={PREVIEW_SELECTED} />
          </Step>
          <Step n={2} title="Run">
            <p>
              Without <code>--to-owner</code>, the destination owner is the Gitea owner. Without <code>--state</code>,
              the state file is <code>./gitcask-migrate-state.json</code>. One failed
              repository does not stop the others; any failure exits nonzero with a list of reasons.
            </p>
            <CodeBlock code={RUN} />
          </Step>
          <Step n={3} title="Resume">
            <p>
              Run the exact same command with the same state path. Completed repositories are skipped; do not edit the
              state file by hand.
            </p>
          </Step>
          <Step n={4} title="Verify">
            <p>
              Keep Gitea read-only. The <code>show-ref</code> diff must be empty and the commit counts must match.
            </p>
            <CodeBlock code={VERIFY} />
          </Step>
          <Step n={5} title="Cut over">
            <p>
              Change client or platform routing only after verification. To roll back, route clients back to Gitea; no
              reverse conversion is needed.
            </p>
          </Step>
        </Steps>
        <Callout title="An existing destination is never overwritten">
          <p>
            Rerunning without the state file is safe. Use a new <code>--to-owner</code> if unrelated repositories already
            hold the target names.
          </p>
        </Callout>
      </Section>

      <Section id="limitations" title="What is not migrated">
        <Definitions
          items={[
            {
              term: 'Platform data',
              children:
                'Issues, pull requests, reviews, releases, packages, wiki, Actions, users, teams, permissions, hooks and repository settings.',
            },
            {
              term: 'Other refs',
              children: (
                <>
                  Only <code>refs/heads/*</code>, <code>refs/tags/*</code> and the symbolic <code>HEAD</code> target.
                  Pull-request refs, notes and remote-tracking refs stay behind.
                </>
              ),
            },
            {
              term: 'Large LFS objects',
              children: (
                <>
                  An object above <code>lfs.max_object_bytes</code> fails that repository.
                </>
              ),
            },
            { term: 'Names', children: 'Must satisfy gitcask’s ASCII naming rules; no rewriting.' },
            { term: 'SSH', children: 'Only the Gitea HTTP clone URL is used.' },
            { term: 'Submodules', children: 'Copied as Git content; referenced repositories and URLs are not migrated.' },
            {
              term: 'One migrator',
              children: 'Never run two migrators with the same state file or overlapping destinations.',
            },
          ]}
        />
      </Section>
    </DocArticle>
  )
}
