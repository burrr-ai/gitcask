/** An ordered procedure. Use only when the order matters. */
export function Steps({ children }: { children: React.ReactNode }) {
  return <ol className="not-prose relative space-y-12 border-l border-border pl-8 md:pl-10">{children}</ol>
}

export function Step({ n, title, children }: { n: number; title: string; children: React.ReactNode }) {
  return (
    <li className="relative">
      <span
        aria-hidden="true"
        className="absolute top-0 -left-[calc(2rem+0.875rem)] grid size-7 place-items-center rounded-pill bg-primary text-label font-semibold text-primary-foreground md:-left-[calc(2.5rem+0.875rem)]"
      >
        {n}
      </span>
      <h3 className="text-title-lg text-foreground">{title}</h3>
      <div className="doc mt-3">{children}</div>
    </li>
  )
}
