'use client'

import { useState } from 'react'

import { Button } from '@/lib/components/ui/button'
import { DOCS } from '@/lib/content/docs'
import { DocsNav } from './docs-nav'

/** Below `lg` the sidebar folds into one disclosure above the page title. */
export function DocsMobileNav({ current }: { current: string }) {
  const [open, setOpen] = useState(false)
  const label = DOCS.find((entry) => entry.slug === current)?.label

  return (
    <div className="lg:hidden">
      <Button
        variant="outline"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
        className="w-full justify-between"
      >
        {label}
        <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth={1.6} aria-hidden="true" className={open ? 'rotate-180' : undefined}>
          <path d="m4 6 4 4 4-4" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </Button>
      {open ? (
        <div className="mt-4 border-b border-border pb-6">
          <DocsNav onNavigate={() => setOpen(false)} />
        </div>
      ) : null}
    </div>
  )
}
