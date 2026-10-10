import type { Metadata } from 'next'
import Link from 'next/link'

import { Flow, Figure } from '@/lib/components/diagram'
import { Callout } from '@/lib/components/docs/callout'
import { CodeBlock } from '@/lib/components/docs/code-block'
import { DocArticle } from '@/lib/components/docs/doc-article'
import { Section } from '@/lib/components/docs/section'
import { Step, Steps } from '@/lib/components/docs/steps'
import { QUICKSTART } from '@/lib/content/quickstart'

export const metadata: Metadata = { title: 'Quickstart' }

const SECTIONS = [
  { id: 'run-it', title: 'Run it locally' },
  { id: 'what-happened', title: 'What the push did' },
  { id: 'production', title: 'Before production' },
]

export default function QuickstartPage() {
  return (
    <DocArticle
      slug="quickstart"
      title="Quickstart"
      lede="One S3-compatible store, one gitcask process, and a push from your own git client in five minutes."
      sections={SECTIONS}
      sources={['README.md', 'compose.yaml', 'gitcask.standalone.toml']}
    >
      <Section id="run-it" title="Run it locally">
        <p>You need Docker with Compose and git. Everything else runs in containers.</p>
        <Steps>
          <Step n={1} title="Start the store and the server">
            <p>
              This starts rustfs, a local S3-compatible store, and one gitcask process on port 8080 using{' '}
              <code>gitcask.standalone.toml</code>.
            </p>
            <CodeBlock code={QUICKSTART.start} />
          </Step>
          <Step n={2} title="Mint a token">
            <p>
              A demo JWT signed with a throwaway key, allowed to administer <code>local/demo</code> for one hour.
            </p>
            <CodeBlock code={QUICKSTART.mint} />
          </Step>
          <Step n={3} title="Create the repository">
            <p>
              Creating a repository is one <code>PUT</code>. gitcask keeps no repository list; your platform does.
            </p>
            <CodeBlock code={QUICKSTART.create} />
          </Step>
          <Step n={4} title="Clone and push">
            <p>
              Git sends the token as the Basic-auth password. The username is ignored.
            </p>
            <CodeBlock code={QUICKSTART.push} />
          </Step>
        </Steps>
      </Section>

      <Section id="what-happened" title="What the push did">
        <p>
          gitcask indexed your pack, uploaded it with its index and a log entry, then made it visible with one
          compare-and-swap of the repository manifest. Only then did git report the new branch.
        </p>
        <Figure
          label="git push, then pack, index and log uploaded, then the manifest compare-and-swap, then ok returned to git."
          className="not-prose rounded-card bg-muted p-6 md:p-8"
        >
          <Flow
            steps={[
              { label: 'git push', tone: 'quiet' },
              { label: 'PUT pack ∥ idx ∥ log', code: true },
              { label: 'CAS manifest.pb', tone: 'brand', code: true },
              { label: 'ok', tone: 'good' },
            ]}
          />
        </Figure>
        <p>
          Stop the server, delete its cache directory and start it again: the repository clones exactly as before,
          because the bucket is the repository. <Link href="/docs/architecture">Architecture</Link> walks through
          every object it wrote.
        </p>
      </Section>

      <Section id="production" title="Before production">
        <ul>
          <li>
            Your platform signs tokens with its own Ed25519 key; gitcask holds only the public key or a JWKS URL. Opaque
            tokens work through introspection instead. See <Link href="/docs/authentication">Authentication</Link>.
          </li>
          <li>
            Point <code>[store]</code> at your S3 bucket and run the published image. See{' '}
            <Link href="/docs/operations">Operations</Link>.
          </li>
        </ul>
        <Callout title="Token lifetimes">
          <p>
            A token pasted into git is saved by the OS credential helper and reused. If it must live for weeks, scope it
            to one repository with the least permission. Backend tokens minted per request can expire in minutes.
          </p>
        </Callout>
      </Section>
    </DocArticle>
  )
}
