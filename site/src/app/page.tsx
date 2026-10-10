import Link from 'next/link'

import { CodeBlock } from '@/lib/components/docs/code-block'
import { CommitDemo } from '@/lib/components/landing/commit-demo'
import { DisposableServers } from '@/lib/components/landing/disposable-servers'
import { Hero } from '@/lib/components/landing/hero'
import { StorageCost } from '@/lib/components/landing/storage-cost'
import { UserSync } from '@/lib/components/landing/user-sync'
import { BrandMark } from '@/lib/components/site/brand-mark'
import { Button } from '@/lib/components/ui/button'
import { REPOSITORY_URL } from '@/lib/content/docs'
import { QUICKSTART_SCRIPT } from '@/lib/content/quickstart'
import { cn } from '@/lib/utils/cn'

export default function LandingPage() {
  return (
    <main>
      <Hero />

      <Scene statement="Storage at bucket prices.">
        <StorageCost />
      </Scene>

      <Scene statement="Lose every server. Lose nothing." tinted>
        <DisposableServers />
      </Scene>

      <Scene statement="Your users stay in your database.">
        <UserSync />
      </Scene>

      <Scene statement="Agents commit without a clone." tinted>
        <CommitDemo />
      </Scene>

      <Scene statement="Yours to run.">
        <div className="grid gap-12 lg:grid-cols-[minmax(0,22rem)_1fr] lg:gap-16">
          <div>
            <p className="text-lede text-soft-foreground">
              Apache-2.0, one binary, your bucket. Any S3-compatible store works, and any number of servers can share it.
            </p>
            <div className="mt-8 flex flex-wrap gap-3">
              <Button asChild size="lg">
                <Link href="/docs/quickstart">Run the quickstart</Link>
              </Button>
              <Button asChild size="lg" variant="outline">
                <a href={REPOSITORY_URL} rel="noreferrer">
                  GitHub
                </a>
              </Button>
            </div>
            <p className="mt-10 text-body text-soft-foreground">
              Building with an agent? Point it at{' '}
              <a href="/llms.txt" className="font-mono text-primary underline-offset-4 hover:underline">
                /llms.txt
              </a>
              .
            </p>
          </div>
          <CodeBlock code={QUICKSTART_SCRIPT} title="Push to a local gitcask in five minutes" className="min-w-0 bg-card" />
        </div>
      </Scene>

      <footer className="mx-auto flex max-w-content flex-col gap-6 px-gutter py-16 md:flex-row md:items-center md:justify-between">
        <Link href="/" className="flex items-center gap-2 text-title-md text-foreground">
          <BrandMark className="size-6 text-primary" />
          gitcask
        </Link>
        <p className="text-label text-soft-foreground">
          The design of Cursor&rsquo;s{' '}
          <a href="https://cursor.com/blog/git-at-any-scale" rel="noreferrer" className="text-foreground underline-offset-4 hover:underline">
            Git at any scale
          </a>
          , forked from{' '}
          <a href="https://github.com/tobi/walgit" rel="noreferrer" className="text-foreground underline-offset-4 hover:underline">
            walgit
          </a>
          . Apache-2.0.
        </p>
      </footer>
    </main>
  )
}

function Scene({
  statement,
  tinted = false,
  children,
}: {
  statement: string
  tinted?: boolean
  children: React.ReactNode
}) {
  return (
    <section className={cn('py-24 md:py-32', tinted && 'bg-muted/60')}>
      <div className="mx-auto max-w-content px-gutter">
        <h2 className="max-w-[18ch] text-statement text-balance text-foreground">{statement}</h2>
        <div className="mt-12 md:mt-16">{children}</div>
      </div>
    </section>
  )
}
