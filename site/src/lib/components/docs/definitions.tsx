/** Term → meaning pairs, e.g. config keys or bucket objects. Terms render as code when `code`. */
export function Definitions({
  items,
  code = false,
}: {
  items: { term: React.ReactNode; children: React.ReactNode }[]
  code?: boolean
}) {
  return (
    <dl className="not-prose divide-y divide-border border-y border-border">
      {items.map((item, index) => (
        <div key={index} className="grid min-w-0 gap-1 py-4 md:grid-cols-[minmax(10rem,14rem)_1fr] md:gap-6">
          <dt className={code ? 'font-mono text-label leading-relaxed font-semibold break-all text-foreground' : 'text-title-md text-foreground'}>
            {item.term}
          </dt>
          <dd className="doc min-w-0 text-soft-foreground">{item.children}</dd>
        </div>
      ))}
    </dl>
  )
}
