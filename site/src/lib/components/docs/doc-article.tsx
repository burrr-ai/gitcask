import Link from 'next/link'

import { DOCS, docHref, findDoc, sourceUrl } from '@/lib/content/docs'
import { DocsMobileNav } from '@/lib/components/site/docs-mobile-nav'

export type DocSectionRef = { id: string; title: string }

/**
 * The frame of every docs page: title, one-sentence lede, the page's sections, the canonical
 * Markdown it restates ("Sources"), and previous/next links. `sections` feeds the on-page outline
 * and must match the `<Section id>`s the page renders.
 */
export function DocArticle({
  slug,
  title,
  lede,
  sections,
  sources,
  children,
}: {
  slug: string
  title: string
  lede: React.ReactNode
  sections: DocSectionRef[]
  /** Repository paths of the Markdown this page restates. */
  sources: string[]
  children: React.ReactNode
}) {
  const index = DOCS.findIndex((entry) => entry.slug === slug)
  const previous = index > 0 ? DOCS[index - 1] : undefined
  const next = index < DOCS.length - 1 ? DOCS[index + 1] : undefined
  if (!findDoc(slug)) throw new Error(`docs page "${slug}" is missing from DOC_GROUPS`)

  return (
    <div className="flex gap-12 pt-6 pb-24 lg:pt-12">
      <article className="min-w-0 max-w-doc flex-1">
        <DocsMobileNav current={slug} />
        <header className="mt-8 lg:mt-0">
          <h1 className="text-display-lg text-balance text-foreground">{title}</h1>
          <p className="mt-4 max-w-[52ch] text-lede text-soft-foreground">{lede}</p>
        </header>

        <div className="mt-14 space-y-20">{children}</div>

        <footer className="mt-24 space-y-10 border-t border-border pt-8">
          <div className="flex flex-wrap items-baseline gap-x-4 gap-y-2 text-label">
            <span className="text-foreground font-semibold">Sources</span>
            {sources.map((path) => (
              <a
                key={path}
                href={sourceUrl(path)}
                rel="noreferrer"
                className="font-mono text-soft-foreground underline-offset-4 hover:text-foreground hover:underline"
              >
                {path}
              </a>
            ))}
          </div>
          <nav aria-label="Pager" className="grid grid-cols-2 gap-4">
            {previous ? (
              <Link href={docHref(previous)} className="group rounded-card p-4 -m-4 hover:bg-accent">
                <span className="block text-label text-soft-foreground">Previous</span>
                <span className="mt-1 block text-title-lg text-foreground">{previous.label}</span>
              </Link>
            ) : (
              <span />
            )}
            {next ? (
              <Link href={docHref(next)} className="group rounded-card p-4 -m-4 text-right hover:bg-accent">
                <span className="block text-label text-soft-foreground">Next</span>
                <span className="mt-1 block text-title-lg text-foreground">{next.label}</span>
              </Link>
            ) : null}
          </nav>
        </footer>
      </article>

      {sections.length > 1 ? (
        <nav
          aria-label="On this page"
          className="sticky top-[calc(var(--appbar-height)+3rem)] hidden max-h-[calc(100dvh-var(--appbar-height)-6rem)] w-52 shrink-0 self-start overflow-y-auto xl:block"
        >
          <p className="text-title-sm text-foreground">On this page</p>
          <ul className="mt-3 space-y-2 border-l border-border">
            {sections.map((section) => (
              <li key={section.id}>
                <a
                  href={`#${section.id}`}
                  className="-ml-px block border-l border-transparent pl-3 text-label text-soft-foreground hover:border-foreground hover:text-foreground"
                >
                  {section.title}
                </a>
              </li>
            ))}
          </ul>
        </nav>
      ) : null}
    </div>
  )
}
