import Link from 'next/link'

import { Button } from '@/lib/components/ui/button'
import { BrandMark } from '@/lib/components/site/brand-mark'
import { Boundary } from '@/lib/components/landing/boundary'
import { Freshness } from '@/lib/components/landing/freshness'
import { IdleCost } from '@/lib/components/landing/idle-cost'
import { PushPath } from '@/lib/components/landing/push-path'
import { Recovery } from '@/lib/components/landing/recovery'
import { Topology } from '@/lib/components/landing/topology'
import { REPOSITORY_URL } from '@/lib/content/docs'
import { quickstartHtml } from '@/lib/content/quickstart'
import { cn } from '@/lib/utils/cn'

export default async function LandingPage() {
  const quickstart = await quickstartHtml()

  return (
    <main>
      <section className="mx-auto grid max-w-content items-center gap-16 px-gutter pt-16 pb-24 md:pt-24 lg:grid-cols-[1.1fr_1fr] lg:pb-32">
        <div>
          <h1 className="max-w-[12ch] text-hero text-balance text-foreground">Git that lives in a bucket.</h1>
          <p className="mt-6 max-w-[34ch] text-lede text-soft-foreground">
            A stateless git server for platforms that create a repository per project.
          </p>
          <div className="mt-10 flex flex-wrap gap-3">
            <Button asChild size="lg">
              <Link href="/docs">Read the docs</Link>
            </Button>
            <Button asChild size="lg" variant="outline">
              <a href={REPOSITORY_URL} rel="noreferrer">
                GitHub
              </a>
            </Button>
          </div>
        </div>
        <Topology />
      </section>

      <Scene statement="A push is one compare-and-swap.">
        <PushPath />
      </Scene>

      <Scene statement="Every read is as fresh as the last push." tinted>
        <Freshness />
      </Scene>

      <Scene statement="Cost follows pushes, not repositories.">
        <IdleCost />
      </Scene>

      <Scene statement="Wipe every server. Lose only warmth." tinted>
        <Recovery />
      </Scene>

      <Scene statement="The git host, without the product around it.">
        <Boundary />
      </Scene>

      <Scene statement="Push to it in five minutes." tinted>
        <div
          className="overflow-x-auto rounded-card bg-card p-6 font-mono text-label leading-relaxed md:p-8 [&_pre]:!bg-transparent"
          dangerouslySetInnerHTML={{ __html: quickstart }}
        />
        <div className="mt-8">
          <Button asChild variant="outline">
            <Link href="/docs#try-it-in-five-minutes">Walk through it</Link>
          </Button>
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
    <section className={cn('py-24 md:py-32', tinted && 'bg-muted')}>
      <div className="mx-auto max-w-content px-gutter">
        <h2 className="max-w-[18ch] text-statement text-balance text-foreground">{statement}</h2>
        <div className="mt-12 md:mt-16">{children}</div>
      </div>
    </section>
  )
}
