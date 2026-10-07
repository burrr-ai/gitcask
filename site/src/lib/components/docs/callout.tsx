import { cn } from '@/lib/utils/cn'

/** A fact the reader must not miss. Use sparingly: one per section at most. */
export function Callout({
  tone = 'note',
  title,
  children,
}: {
  tone?: 'note' | 'warning'
  title: string
  children: React.ReactNode
}) {
  return (
    <aside
      className={cn(
        'not-prose rounded-control border-l-[3px] px-5 py-4',
        tone === 'note' ? 'border-primary bg-primary-surface' : 'border-warning bg-warning-surface'
      )}
    >
      <p className={cn('text-title-md', tone === 'note' ? 'text-primary-ink' : 'text-warning-surface-foreground')}>
        {title}
      </p>
      <div className="doc mt-1.5 text-foreground">{children}</div>
    </aside>
  )
}
