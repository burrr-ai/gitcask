import Link from 'next/link'

import { Figure } from './diagram'

/**
 * Store requests over five idle minutes, from the load spikes recorded in docs/DIRECTION.md §5
 * (items 08 and 11): 118 at 100 and 1,000 repositories, 120 at 50,000.
 */
const SPIKE = [
  { repositories: 100, requests: 118 },
  { repositories: 1_000, requests: 118 },
  { repositories: 50_000, requests: 120 },
]

const CHART_MAX = 140

export function IdleCost() {
  return (
    <Figure label="Store requests during five idle minutes stay flat as repositories grow: 118 at 100 repositories, 118 at 1,000, and 120 at 50,000.">
      <div className="flex items-end gap-4 md:gap-10" aria-hidden="true">
        {SPIKE.map((point) => (
          <div key={point.repositories} className="group relative flex flex-1 flex-col items-center">
            <span className="text-display-sm text-foreground tabular-nums">{point.requests}</span>
            <div className="mt-3 flex h-56 w-full items-end justify-center md:h-72">
              <div
                className="w-full max-w-24 rounded-t-[4px] bg-primary transition-opacity duration-fast group-hover:opacity-85"
                style={{ height: `${(point.requests / CHART_MAX) * 100}%` }}
              />
            </div>
            <span
              role="tooltip"
              className="pointer-events-none absolute top-14 hidden md:block left-1/2 z-raised -translate-x-1/2 rounded-control bg-foreground px-3 py-2 text-label whitespace-nowrap text-background opacity-0 shadow-md transition-opacity duration-fast group-hover:opacity-100"
            >
              {point.repositories.toLocaleString('en-US')} repositories · {point.requests} requests
            </span>
          </div>
        ))}
      </div>
      <div className="h-px bg-border-strong" aria-hidden="true" />
      <div className="mt-3 flex gap-4 md:gap-10" aria-hidden="true">
        {SPIKE.map((point) => (
          <span key={point.repositories} className="flex-1 text-center text-title-md text-foreground tabular-nums">
            {point.repositories.toLocaleString('en-US')}
          </span>
        ))}
      </div>
      <div className="mt-8 flex flex-wrap items-baseline justify-between gap-x-6 gap-y-2 text-label">
        <span className="text-soft-foreground">Store requests in five idle minutes, by repository count</span>
        <Link href="/docs/direction#5-progress" className="text-primary underline-offset-4 hover:underline">
          Load spike results
        </Link>
      </div>

      <table className="sr-only">
        <thead>
          <tr>
            <th>Repositories</th>
            <th>Store requests in five idle minutes</th>
          </tr>
        </thead>
        <tbody>
          {SPIKE.map((point) => (
            <tr key={point.repositories}>
              <td>{point.repositories}</td>
              <td>{point.requests}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </Figure>
  )
}
