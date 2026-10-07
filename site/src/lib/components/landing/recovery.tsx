import { BucketGlyph, Chip, Figure, ServerGlyph } from './diagram'

type Instance = { tone: 'plain' | 'gone' | 'brand'; label: string }

/** README "How it works" and GOAL acceptance: a cold instance serves refs in under a second. */
const STAGES: { title: string; instances: Instance[] }[] = [
  {
    title: 'Serving',
    instances: [
      { tone: 'plain', label: 'serve' },
      { tone: 'plain', label: 'serve' },
      { tone: 'plain', label: 'maintain' },
    ],
  },
  {
    title: 'Every instance gone',
    instances: [
      { tone: 'gone', label: 'serve' },
      { tone: 'gone', label: 'serve' },
      { tone: 'gone', label: 'maintain' },
    ],
  },
  {
    title: 'New instance, refs in < 1 s',
    instances: [{ tone: 'brand', label: 'serve' }],
  },
]

export function Recovery() {
  return (
    <Figure label="Three instances serve from a bucket. All of them disappear; the bucket is untouched. A new instance pointed at the bucket serves refs in under a second.">
      <ol className="grid gap-10 md:grid-cols-3 md:gap-6">
        {STAGES.map((stage) => (
          <li key={stage.title} className="flex flex-col">
            <span className="text-title-md text-foreground">{stage.title}</span>
            <div className="mt-5 flex min-h-11 flex-wrap gap-2">
              {stage.instances.map((instance, index) => (
                <Chip key={index} tone={instance.tone} className="px-3">
                  <ServerGlyph />
                  {instance.label}
                </Chip>
              ))}
            </div>
            <div className="mt-3 flex items-center gap-2 rounded-control bg-muted px-3.5 py-3 font-mono text-label text-foreground">
              <BucketGlyph className="text-primary" />
              bucket
            </div>
          </li>
        ))}
      </ol>
    </Figure>
  )
}
