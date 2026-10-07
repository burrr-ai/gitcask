import { Arrow, Chip, Figure } from './diagram'

/**
 * The happy-path publish from docs/ROUNDTRIPS.md §2: freshness GET → pack ∥ idx ∥ log PUT →
 * manifest CAS. Five store requests in three sequential rounds; the client hears `ok` only after
 * the CAS.
 */
const ROUNDS: { label: string; steps: string[]; commit?: boolean }[] = [
  { label: 'Round 1', steps: ['GET manifest.pb'] },
  { label: 'Round 2', steps: ['PUT pack', 'PUT idx', 'PUT log'] },
  { label: 'Round 3', steps: ['CAS manifest.pb'], commit: true },
]

export function PushPath() {
  return (
    <Figure label="A push: one manifest GET, then the pack, index and log entry uploaded in parallel, then one compare-and-swap of the manifest. The client is acknowledged after the swap.">
      <div className="flex flex-col items-stretch gap-4 md:flex-row md:items-center md:gap-5">
        <Chip tone="quiet" className="md:self-center">git push</Chip>
        <Arrow className="self-center" />
        {ROUNDS.map((round, index) => (
          <div key={round.label} className="contents">
            <div className="flex flex-1 flex-col gap-2">
              <span className="font-mono text-label text-muted-foreground">{round.label}</span>
              {round.steps.map((step) => (
                <Chip key={step} tone={round.commit ? 'brand' : 'plain'}>
                  {step}
                </Chip>
              ))}
            </div>
            {index < ROUNDS.length - 1 ? <Arrow className="self-center md:mt-7" /> : null}
          </div>
        ))}
        <Arrow className="self-center md:mt-7" />
        <Chip className="border-success-border bg-success-surface text-success-surface-foreground md:mt-7">
          ok
        </Chip>
      </div>
    </Figure>
  )
}
