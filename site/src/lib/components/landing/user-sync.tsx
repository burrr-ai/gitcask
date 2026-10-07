'use client'

import { useRef, useState } from 'react'

import { SegmentedControl } from '@/lib/components/ui/segmented-control'
import { cn } from '@/lib/utils/cn'

/**
 * docs/PRODUCT.md §6: a git server with its own accounts must mirror every sign-up, rename and
 * deletion; gitcask reads one signed token and stores nothing about the user.
 */
type View = 'accounts' | 'gitcask'

const MIRRORED: { event: string; yours: string; theirs: string; broken?: boolean }[] = [
  { event: 'Sign-up', yours: 'mina', theirs: 'mina' },
  { event: 'Rename', yours: 'jun → junho', theirs: 'jun', broken: true },
  { event: 'Deletion', yours: 'sora (deleted)', theirs: 'sora', broken: true },
  { event: 'Suspension', yours: 'kai (suspended)', theirs: 'kai', broken: true },
]

export function UserSync() {
  const [view, setView] = useState<View>('accounts')

  return (
    <div>
      <SegmentedControl
        aria-label="Compare"
        size="md"
        options={[
          { label: 'Git server with its own accounts', value: 'accounts' as View },
          { label: 'gitcask', value: 'gitcask' as View },
        ]}
        value={view}
        onValueChange={setView}
      />

      <div className="mt-10 min-h-[26rem]">
        {view === 'accounts' ? <Mirrored /> : <SignedToken />}
      </div>
    </div>
  )
}

function Mirrored() {
  return (
    <div className="animate-in fade-in grid gap-8 duration-base lg:grid-cols-[1fr_minmax(0,20rem)] lg:items-start">
      <div className="overflow-hidden rounded-card border border-border">
        <div className="grid grid-cols-[7rem_1fr_1fr] bg-muted px-5 py-3 text-label font-semibold text-foreground md:grid-cols-[9rem_1fr_1fr]">
          <span>Event</span>
          <span>Your database</span>
          <span>Git server copy</span>
        </div>
        {MIRRORED.map((row) => (
          <div
            key={row.event}
            className={cn(
              'grid grid-cols-[7rem_1fr_1fr] items-center border-t border-border px-5 py-4 text-body md:grid-cols-[9rem_1fr_1fr]',
              row.broken && 'bg-destructive-surface/60'
            )}
          >
            <span className="text-foreground">{row.event}</span>
            <span className="text-soft-foreground">{row.yours}</span>
            <span className={row.broken ? 'font-semibold text-destructive-surface-foreground' : 'text-soft-foreground'}>
              {row.theirs}
            </span>
          </div>
        ))}
      </div>
      <p className="text-lede text-soft-foreground">
        Every account lives twice. Each rename, deletion and suspension must be copied across, and every miss orphans
        a repository.
      </p>
    </div>
  )
}

function SignedToken() {
  const card = useRef<HTMLDivElement>(null)
  const [tilt, setTilt] = useState({ x: 0, y: 0 })

  function onMove(event: React.PointerEvent) {
    if (event.pointerType !== 'mouse' || !card.current) return
    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return
    const rect = card.current.getBoundingClientRect()
    const x = (event.clientX - rect.left) / rect.width - 0.5
    const y = (event.clientY - rect.top) / rect.height - 0.5
    setTilt({ x: -y * 8, y: x * 10 })
  }

  return (
    <div className="animate-in fade-in grid gap-8 duration-base lg:grid-cols-[1fr_minmax(0,20rem)] lg:items-center">
      <div className="[perspective:1200px]" onPointerMove={onMove} onPointerLeave={() => setTilt({ x: 0, y: 0 })}>
        <div
          ref={card}
          className="relative mx-auto max-w-xl overflow-hidden rounded-sheet border border-primary/50 bg-card p-7 shadow-lg transition-transform duration-base ease-out md:p-9"
          style={{ transform: `rotateX(${tilt.x}deg) rotateY(${tilt.y}deg)` }}
        >
          <div
            aria-hidden="true"
            className="pointer-events-none absolute -top-24 -right-24 size-64 rounded-full bg-primary/15 blur-3xl"
          />
          <div className="flex items-center justify-between gap-4">
            <p className="text-title-lg text-foreground">Signed by your platform</p>
            <span className="rounded-pill bg-primary-surface px-3 py-1 text-label font-semibold text-primary-ink">EdDSA</span>
          </div>
          <dl className="mt-7 space-y-4 font-mono text-body">
            <Claim name="sub" value={'"u_8f2c41"'} note="an opaque principal" />
            <Claim name="scopes" value={'["acme/site:write"]'} note="repository and permission" />
            <Claim name="exp" value="1791676800" note="when it stops working" />
          </dl>
          <p className="mt-7 border-t border-border pt-5 text-body text-soft-foreground">
            gitcask checks the signature with your public key and keeps nothing.
          </p>
        </div>
      </div>
      <p className="text-lede text-soft-foreground">
        Users, teams and revocation stay in your database. gitcask sees an opaque principal and a scope, verifies, and
        forgets.
      </p>
    </div>
  )
}

function Claim({ name, value, note }: { name: string; value: string; note: string }) {
  return (
    <div className="grid gap-1 sm:grid-cols-[6rem_1fr] sm:items-baseline">
      <dt className="text-primary">{name}</dt>
      <dd className="min-w-0">
        <span className="break-all text-foreground">{value}</span>
        <span className="mt-0.5 block font-sans text-label text-soft-foreground">{note}</span>
      </dd>
    </div>
  )
}
