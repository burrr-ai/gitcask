'use client'

import Link from 'next/link'
import { usePathname } from 'next/navigation'

import { DOC_GROUPS, docHref } from '@/lib/content/docs'
import { cn } from '@/lib/utils/cn'

export function DocsNav({ onNavigate }: { onNavigate?: () => void }) {
  const pathname = usePathname()

  return (
    <nav aria-label="Documentation" className="space-y-8">
      {DOC_GROUPS.map((group) => (
        <div key={group.title}>
          <p className="text-title-sm text-foreground">{group.title}</p>
          <ul className="mt-2 space-y-0.5">
            {group.entries.map((entry) => {
              const href = docHref(entry)
              const active = pathname === href
              return (
                <li key={href}>
                  <Link
                    href={href}
                    onClick={onNavigate}
                    aria-current={active ? 'page' : undefined}
                    className={cn(
                      'block rounded-control px-3 py-1.5 text-label transition-colors duration-fast',
                      active
                        ? 'bg-primary-surface font-semibold text-primary-ink'
                        : 'text-soft-foreground hover:bg-accent hover:text-foreground'
                    )}
                  >
                    {entry.label}
                  </Link>
                </li>
              )
            })}
          </ul>
        </div>
      ))}
    </nav>
  )
}
