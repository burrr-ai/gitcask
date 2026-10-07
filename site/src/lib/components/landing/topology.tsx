import { BucketGlyph, Chip, Figure, ServerGlyph } from './diagram'

const ROLES = ['serve', 'serve', 'maintain']

const FILES: { name: string; note?: string }[] = [
  { name: 'manifest.pb', note: 'CAS' },
  { name: 'log/000042.pb' },
  { name: 'wal/3f9c…e1.pack' },
  { name: 'checkpoints/000040/' },
]

/** Disposable instances above, the bucket that is the repository below. */
export function Topology() {
  return (
    <Figure label="Three stateless gitcask instances, all reading and writing one S3 bucket that holds each repository as a manifest, a log and immutable packs.">
      <div className="grid grid-cols-3 gap-3">
        {ROLES.map((role, index) => (
          <Chip key={index} className="w-full">
            <ServerGlyph className="text-soft-foreground" />
            {role}
          </Chip>
        ))}
      </div>

      <svg viewBox="0 0 300 56" preserveAspectRatio="none" aria-hidden="true" className="block h-14 w-full text-border-strong">
        {[50, 150, 250].map((x) => (
          <path
            key={x}
            d={`M${x} 0 C${x} 30, 150 26, 150 56`}
            fill="none"
            stroke="currentColor"
            strokeWidth={1.5}
            vectorEffect="non-scaling-stroke"
          />
        ))}
      </svg>

      <div className="rounded-card border border-border-strong/60 bg-card p-5 font-mono text-label md:p-6">
        <div className="flex items-center gap-2 text-foreground">
          <BucketGlyph className="size-5 text-primary" />
          <span className="font-semibold">s3://bucket</span>
        </div>
        <div className="mt-4 text-soft-foreground">repos/acme/site/</div>
        <ul className="mt-2 space-y-2 border-l border-border pl-4">
          {FILES.map((file) => (
            <li key={file.name} className="flex items-center gap-3">
              <span className={file.note ? 'font-semibold text-foreground' : 'text-soft-foreground'}>
                {file.name}
              </span>
              {file.note ? (
                <span className="rounded-pill bg-primary px-2 py-0.5 text-caption font-semibold text-primary-foreground">
                  {file.note}
                </span>
              ) : null}
            </li>
          ))}
        </ul>
      </div>
    </Figure>
  )
}
