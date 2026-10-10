'use client'

import { useEffect, useRef, useState } from 'react'

import { BucketGlyph, ServerGlyph } from '@/lib/components/diagram'
import { Button } from '@/lib/components/ui/button'
import { cn } from '@/lib/utils/cn'

/**
 * README "How it works" and GOAL acceptance: wipe every instance and the bucket still holds every
 * repository; a fresh instance serves refs with one manifest GET, then checkpoint refs and the log
 * tail in parallel — under a second.
 */
type Phase = 'serving' | 'wiped' | 'booting' | 'restored'

const COLD_START = ['GET manifest.pb', 'checkpoint refs ∥ log tail', 'serving refs']

export function DisposableServers() {
  const [phase, setPhase] = useState<Phase>('serving')
  const [step, setStep] = useState(-1)
  const timers = useRef<number[]>([])

  useEffect(() => () => timers.current.forEach(clearTimeout), [])

  const later = (ms: number, run: () => void) => {
    timers.current.push(window.setTimeout(run, ms))
  }

  function wipe() {
    setPhase('wiped')
    setStep(-1)
  }

  function boot() {
    setPhase('booting')
    setStep(0)
    later(260, () => setStep(1))
    later(560, () => setStep(2))
    later(800, () => setPhase('restored'))
  }

  function reset() {
    timers.current.forEach(clearTimeout)
    setPhase('serving')
    setStep(-1)
  }

  const servers = phase === 'serving' ? 3 : phase === 'wiped' ? 0 : 1

  return (
    <div className="grid gap-10 lg:grid-cols-[1fr_minmax(0,22rem)] lg:items-center lg:gap-16">
      <div
        role="img"
        aria-label={
          phase === 'wiped'
            ? 'No servers are running. The bucket still holds every repository.'
            : phase === 'serving'
              ? 'Three servers serve from one bucket.'
              : 'A new server reads the manifest, then checkpoint refs and the log tail, then serves.'
        }
        className="relative rounded-card bg-muted px-6 pt-8 pb-6 md:px-10 md:pt-10"
      >
        <div className="grid h-24 grid-cols-3 gap-3 md:gap-5">
          {[0, 1, 2].map((slot) => {
            const present = slot < servers
            return (
              <div
                key={slot}
                className={cn(
                  'flex items-center justify-center gap-2 rounded-control border bg-card text-label transition-all duration-slower ease-out',
                  present
                    ? 'translate-y-0 border-border-strong/60 opacity-100'
                    : 'translate-y-6 scale-95 border-dashed border-border-strong opacity-0',
                  phase === 'restored' && slot === 0 && 'border-primary text-primary-ink'
                )}
              >
                <ServerGlyph className="size-5 text-soft-foreground" />
                <span className="hidden text-foreground sm:inline">{slot === 0 && phase !== 'serving' ? 'new server' : 'server'}</span>
              </div>
            )
          })}
        </div>

        <div className="relative my-4 h-10" aria-hidden="true">
          {[16.6, 50, 83.3].map((left, slot) => (
            <span
              key={left}
              className={cn(
                'absolute top-0 h-full w-px bg-border-strong transition-opacity duration-slow',
                slot < servers ? 'opacity-100' : 'opacity-0'
              )}
              style={{ left: `${left}%` }}
            />
          ))}
        </div>

        <div
          className={cn(
            'flex items-center gap-4 rounded-card border border-primary bg-primary-surface px-5 py-5 transition-shadow duration-slower',
            phase === 'wiped' && 'shadow-[0_0_0_6px_color-mix(in_srgb,var(--primary)_18%,transparent)]'
          )}
        >
          <BucketGlyph className="size-7 shrink-0 text-primary" />
          <div className="min-w-0">
            <p className="text-title-lg text-primary-ink">Your bucket</p>
            <p className="text-body text-primary-ink/80">Every repository, every push, intact</p>
          </div>
        </div>

        <ol className="mt-6 flex flex-col gap-2 md:flex-row md:items-center md:gap-3" aria-hidden={step < 0}>
          {COLD_START.map((label, index) => (
            <li
              key={label}
              className={cn(
                'rounded-control border px-3.5 py-2 font-mono text-label transition-all duration-base',
                step >= index
                  ? index === COLD_START.length - 1
                    ? 'border-success-border bg-success-surface text-success-surface-foreground'
                    : 'border-border-strong/60 bg-card text-foreground'
                  : 'border-transparent text-transparent'
              )}
            >
              {label}
            </li>
          ))}
        </ol>
      </div>

      <div>
        <p className="text-lede text-soft-foreground">
          {phase === 'serving' && 'Three servers, one bucket. None of them holds anything the bucket does not.'}
          {phase === 'wiped' && 'Every server is gone. Nothing was lost: the bucket is the repository.'}
          {phase === 'booting' && 'A new server reads one small manifest, then the refs. No replay, no sync.'}
          {phase === 'restored' && 'Serving refs in under a second, on a server that did not exist a moment ago.'}
        </p>
        <div className="mt-8">
          {phase === 'serving' && (
            <Button size="lg" variant="outline" onClick={wipe}>
              Remove every server
            </Button>
          )}
          {phase === 'wiped' && (
            <Button size="lg" onClick={boot}>
              Start a new server
            </Button>
          )}
          {(phase === 'booting' || phase === 'restored') && (
            <Button size="lg" variant="outline" onClick={reset} disabled={phase === 'booting'}>
              Run it again
            </Button>
          )}
        </div>
      </div>
    </div>
  )
}
