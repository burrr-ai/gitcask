import { cn } from '@/lib/utils/cn'

/**
 * One topic of a docs page. Prose children (p, ul, ol, a, code, strong) get the `.doc` reading
 * styles; diagrams, tables and code blocks bring their own.
 */
export function Section({
  id,
  title,
  className,
  children,
}: {
  id: string
  title: string
  className?: string
  children: React.ReactNode
}) {
  return (
    <section id={id} className={cn('scroll-mt-24', className)}>
      <h2 className="text-display-sm text-balance text-foreground">
        <a href={`#${id}`} className="hover:text-primary">
          {title}
        </a>
      </h2>
      <div className="doc mt-5">{children}</div>
    </section>
  )
}

/** A sub-topic inside a Section. */
export function SubSection({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="pt-4">
      <h3 className="text-title-lg text-foreground">{title}</h3>
      <div className="doc mt-3">{children}</div>
    </div>
  )
}
