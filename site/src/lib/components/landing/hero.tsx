'use client'

import Link from 'next/link'
import { useCallback, useState } from 'react'

import { Button } from '@/lib/components/ui/button'
import { REPOSITORY_URL } from '@/lib/content/docs'
import { RepositoryField, type PushSample } from './repository-field'

const NAMES = ['mina/landing-page', 'u_8f2c41/todo-app', 'kai/portfolio', 'acme/site', 'junho/game-jam', 'sora/recipes', 'team-42/dashboard']

export function Hero() {
  const [pushes, setPushes] = useState(0)
  const [sample, setSample] = useState<PushSample | null>(null)
  const onPush = useCallback((count: number) => setPushes(count), [])
  const onSample = useCallback((next: PushSample) => setSample(next), [])

  return (
    <section className="relative isolate overflow-hidden border-b border-border">
      <div className="absolute inset-0 -z-10">
        <RepositoryField onPush={onPush} onSample={onSample} />
        {/* Legibility veil behind the copy; the field stays visible to the right and below. */}
        <div className="pointer-events-none absolute inset-0 bg-gradient-to-b from-background/85 via-background/70 to-background/30 md:bg-gradient-to-r md:from-background md:from-30% md:via-background/70 md:via-50% md:to-transparent md:to-75%" />
        <div className="pointer-events-none absolute inset-x-0 bottom-0 h-24 bg-gradient-to-t from-background to-transparent" />
        {sample ? (
          <div
            key={sample.id}
            aria-hidden="true"
            className="pointer-events-none absolute animate-out fade-out fill-mode-forwards delay-1500 duration-500"
            style={{ left: sample.x, top: sample.y }}
          >
            <span className="absolute -top-px -left-px size-2 -translate-x-1/2 -translate-y-1/2 animate-ping rounded-full bg-primary" />
            <span className="absolute left-3 -translate-y-1/2 animate-in fade-in slide-in-from-left-1 rounded-control border border-primary/40 bg-background/90 px-2.5 py-1 font-mono text-label whitespace-nowrap text-foreground shadow-md backdrop-blur">
              {NAMES[sample.id % NAMES.length]} <span className="text-primary">pushed</span>
            </span>
          </div>
        ) : null}
      </div>

      <div className="pointer-events-none mx-auto flex min-h-[min(46rem,calc(100svh-var(--appbar-height)))] max-w-content flex-col justify-between gap-16 px-gutter pt-20 pb-10 md:pt-28">
        <div className="max-w-2xl">
          <h1 className="text-hero text-balance text-foreground">Pay for pushes, not repositories.</h1>
          <p className="mt-6 max-w-[38ch] text-lede text-soft-foreground">
            Keep every user&rsquo;s projects in your own S3 bucket. An idle repository costs its bytes and nothing else.
          </p>
          <div className="pointer-events-auto mt-10 flex flex-wrap gap-3">
            <Button asChild size="lg">
              <Link href="/docs">Read the docs</Link>
            </Button>
            <Button asChild size="lg" variant="outline" className="bg-background/70 backdrop-blur">
              <a href={REPOSITORY_URL} rel="noreferrer">
                GitHub
              </a>
            </Button>
          </div>
        </div>

        <dl className="flex flex-wrap items-end gap-x-10 gap-y-4">
          <div className="flex items-center gap-3">
            <span aria-hidden="true" className="size-3 rounded-[3px] bg-accent ring-1 ring-border-strong/60" />
            <dt className="text-label text-soft-foreground">Idle repository</dt>
            <dd className="sr-only">costs its bytes in the bucket</dd>
          </div>
          <div className="flex items-center gap-3">
            <span aria-hidden="true" className="size-3 rounded-[3px] bg-primary" />
            <dt className="text-label text-soft-foreground">Being pushed to</dt>
            <dd className="sr-only">the only repositories gitcask does work for</dd>
          </div>
          <div className="flex items-baseline gap-2">
            <dd className="text-display-sm text-foreground tabular-nums" aria-live="off">
              {pushes}
            </dd>
            <dt className="text-label text-soft-foreground">pushes in the last five seconds</dt>
          </div>
        </dl>
      </div>
    </section>
  )
}
