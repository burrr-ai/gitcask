'use client'

import { useEffect, useRef, useState } from 'react'

import { Button } from '@/lib/components/ui/button'
import { cn } from '@/lib/utils/cn'

/**
 * `POST /{owner}/{repo}/api/commits` with the real request and response fields
 * (crates/gitcask-server/src/web/api/commit.rs). One request is one commit: no clone, no working
 * directory, and `expected_head_oid` makes it a compare-and-swap on the branch.
 */
const CHANGES = [
  { message: 'Add pricing page', path: 'app/pricing/page.tsx', content: 'ZXhwb3J0IGRlZmF1bHQg' },
  { message: 'Tighten mobile nav', path: 'app/nav.tsx', content: 'aW1wb3J0IHsgdXNlU3Rh' },
  { message: 'Fix checkout total', path: 'lib/cart.ts', content: 'Y29uc3QgdG90YWwgPSAo' },
  { message: 'Add dark theme', path: 'app/theme.css', content: 'OnJvb3QgeyAtLWJyYW5k' },
]

type Commit = { oid: string; message: string; fresh?: boolean }

function oid() {
  return Array.from({ length: 40 }, () => '0123456789abcdef'[Math.floor(Math.random() * 16)]).join('')
}

const short = (value: string) => `${value.slice(0, 7)}…`

export function CommitDemo() {
  const [commits, setCommits] = useState<Commit[]>([
    { oid: 'c41f9e2a7b3d58e0f1a2b3c4d5e6f708192a3b4c', message: 'Initial project' },
  ])
  const [seq, setSeq] = useState(41)
  const [turn, setTurn] = useState(0)
  const [sending, setSending] = useState(false)
  const [response, setResponse] = useState<{ oid: string; seq: number } | null>(null)
  const timers = useRef<number[]>([])
  useEffect(() => () => timers.current.forEach(clearTimeout), [])

  const head = commits[0]
  const change = CHANGES[turn % CHANGES.length]

  function send() {
    if (sending) return
    setSending(true)
    setResponse(null)
    const next = oid()
    timers.current.push(
      window.setTimeout(() => {
        setCommits((list) => [{ oid: next, message: change.message, fresh: true }, ...list.map((c) => ({ ...c, fresh: false }))].slice(0, 5))
        setSeq((value) => value + 1)
        setResponse({ oid: next, seq: seq + 1 })
        setSending(false)
        setTurn((value) => value + 1)
      }, 650)
    )
  }

  const request = `POST /acme/site/api/commits
{
  "branch": "main",
  "message": "${change.message}",
  "expected_head_oid": "${short(head.oid)}",
  "committer": {
    "name": "agent",
    "email": "agent@acme.dev",
    "when": "2026-10-07T09:12:00Z"
  },
  "changes": [{
    "op": "upsert",
    "path": "${change.path}",
    "content": "${change.content}…",
    "mode": "100644"
  }]
}`

  return (
    <div className="grid gap-8 lg:grid-cols-[minmax(0,1.15fr)_minmax(0,1fr)] lg:gap-12">
      <div
        className={cn(
          'overflow-hidden rounded-card border bg-card transition-[border-color,box-shadow] duration-base',
          sending ? 'border-primary shadow-[0_0_0_4px_color-mix(in_srgb,var(--primary)_16%,transparent)]' : 'border-border'
        )}
      >
        <div className="flex items-center justify-between gap-4 border-b border-border px-5 py-3">
          <p className="text-title-sm text-foreground">Request from your agent</p>
          <Button size="sm" onClick={send} disabled={sending}>
            {sending ? 'Committing' : 'Send'}
          </Button>
        </div>
        <pre className="overflow-x-auto px-5 py-4 font-mono text-body-sm leading-relaxed text-foreground">
          {request}
        </pre>
      </div>

      <div className="flex flex-col gap-6">
        <div className="rounded-card bg-muted p-5 md:p-6">
          <p className="text-title-sm text-foreground">
            <span className="font-mono">main</span>
          </p>
          <ol className="relative mt-4 space-y-3 before:absolute before:top-2 before:bottom-2 before:left-[7px] before:w-px before:bg-border-strong">
            {commits.map((commit) => (
              <li
                key={commit.oid}
                className={cn('relative flex items-center gap-4 pl-7', commit.fresh && 'animate-in fade-in slide-in-from-top-2 duration-slow')}
              >
                <span
                  aria-hidden="true"
                  className={cn(
                    'absolute left-0 size-[15px] rounded-full border-2',
                    commit === head ? 'border-primary bg-primary' : 'border-border-strong bg-background'
                  )}
                />
                <span className="font-mono text-label text-soft-foreground">{commit.oid.slice(0, 7)}</span>
                <span className={cn('truncate text-body', commit === head ? 'font-semibold text-foreground' : 'text-soft-foreground')}>
                  {commit.message}
                </span>
              </li>
            ))}
          </ol>
        </div>

        <div
          aria-live="polite"
          className={cn(
            'rounded-card border px-5 py-4 transition-opacity duration-base',
            response ? 'border-success-border bg-success-surface opacity-100' : 'border-dashed border-border-strong opacity-60'
          )}
        >
          {response ? (
            <pre className="overflow-x-auto font-mono text-body-sm leading-relaxed text-success-surface-foreground">{`201 Created
{ "ref": "refs/heads/main",
  "oid": "${short(response.oid)}",
  "commit_oid": "${short(response.oid)}",
  "seq": ${response.seq} }`}</pre>
          ) : (
            <p className="text-body text-soft-foreground">Press Send. One request becomes one commit on main.</p>
          )}
        </div>
      </div>
    </div>
  )
}
