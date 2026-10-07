import { Arrow, Chip, Figure } from './diagram'

/** Every read starts with a conditional GET of the manifest (AGENTS.md §2.3, ROUNDTRIPS.md §1). */
const LANES: { title: string; steps: { text: string; tone?: 'brand' | 'plain' }[] }[] = [
  {
    title: 'Nothing new',
    steps: [{ text: 'If-None-Match: manifest' }, { text: '304 · 15 ms', tone: 'brand' }, { text: 'serve' }],
  },
  {
    title: 'After a push',
    steps: [
      { text: 'If-None-Match: manifest' },
      { text: '200 · seq 42', tone: 'brand' },
      { text: 'apply log tail' },
      { text: 'serve' },
    ],
  },
]

export function Freshness() {
  return (
    <Figure label="Each read revalidates the manifest. When nothing changed the store answers 304 in about 15 milliseconds; after a push it answers 200 and the instance applies the new log entries before serving.">
      <div className="space-y-8">
        {LANES.map((lane) => (
          <div key={lane.title} className="grid gap-3 md:grid-cols-[10rem_1fr] md:items-center">
            <span className="text-title-md text-foreground">{lane.title}</span>
            <div className="flex flex-col items-start gap-2 md:flex-row md:flex-wrap md:items-center md:gap-3">
              {lane.steps.map((step, index) => (
                <div key={step.text} className="flex flex-col items-start gap-2 md:flex-row md:items-center md:gap-3">
                  {index > 0 ? <Arrow className="ml-4 md:ml-0" /> : null}
                  <Chip tone={step.tone}>{step.text}</Chip>
                </div>
              ))}
            </div>
          </div>
        ))}
      </div>
    </Figure>
  )
}
