import { DocsMobileNav } from './docs-mobile-nav'
import { renderDoc } from '@/lib/content/render'
import { sourceUrl, type DocEntry } from '@/lib/content/docs'

export async function DocPage({ entry }: { entry: DocEntry }) {
  const { title, headings, body } = await renderDoc(entry)

  return (
    <div className="flex gap-12 pt-6 pb-24 lg:pt-10">
      <article className="min-w-0 max-w-doc flex-1">
        <DocsMobileNav current={entry.slug} />
        <h1 className="mt-6 text-display-lg text-balance text-foreground lg:mt-0">{title}</h1>
        <div className="doc mt-8">{body}</div>
        <a
          href={sourceUrl(entry.source)}
          rel="noreferrer"
          className="mt-16 inline-block text-label text-soft-foreground underline-offset-4 hover:text-foreground hover:underline"
        >
          {entry.source} on GitHub
        </a>
      </article>

      {headings.length > 1 ? (
        <nav aria-label="On this page" className="sticky top-[calc(var(--appbar-height)+2.5rem)] hidden max-h-[calc(100dvh-var(--appbar-height)-5rem)] w-52 shrink-0 self-start overflow-y-auto xl:block">
          <p className="text-title-sm text-foreground">On this page</p>
          <ul className="mt-3 space-y-2 border-l border-border">
            {headings.map((heading) => (
              <li key={heading.id}>
                <a
                  href={`#${heading.id}`}
                  className="-ml-px block border-l border-transparent pl-3 text-label text-soft-foreground hover:border-foreground hover:text-foreground"
                >
                  {heading.text}
                </a>
              </li>
            ))}
          </ul>
        </nav>
      ) : null}
    </div>
  )
}
