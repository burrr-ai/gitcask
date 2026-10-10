import { DocsNav } from '@/lib/components/site/docs-nav'

export default function DocsLayout({ children }: { children: React.ReactNode }) {
  return (
    <div className="mx-auto flex max-w-content gap-12 px-gutter">
      <aside className="sticky top-appbar hidden h-[calc(100dvh-var(--appbar-height))] w-56 shrink-0 overflow-y-auto py-10 lg:block">
        <DocsNav />
      </aside>
      <div className="min-w-0 flex-1">{children}</div>
    </div>
  )
}
