import { BucketGlyph, Chip, Figure } from './diagram'

/** docs/PRODUCT.md §1 and §4: gitcask owns the bytes, the platform owns the product. */
const PLATFORM = ['Users & sign-in', 'Repository list', 'Permissions', 'CI', 'Reviews', 'UI']
const LINKS = ['signed token', 'JSON API', 'webhook']
const GITCASK = ['Smart HTTP v0/v2', 'LFS', 'JSON API', 'Webhooks', 'Initialize', 'Import']

export function Boundary() {
  return (
    <Figure label="Three layers. Your platform owns users, the repository list, permissions, CI, reviews and UI. It talks to gitcask with signed tokens, the JSON API and webhooks. gitcask serves git, LFS, the API and events, and stores everything in a bucket.">
      <div className="rounded-card border border-dashed border-border-strong p-5 md:p-7">
        <span className="text-title-lg text-foreground">Your platform</span>
        <div className="mt-4 flex flex-wrap gap-2">
          {PLATFORM.map((item) => (
            <Chip key={item} tone="quiet" className="font-sans">
              {item}
            </Chip>
          ))}
        </div>
      </div>

      <div className="flex flex-wrap justify-center gap-x-10 gap-y-2 py-5">
        {LINKS.map((link) => (
          <span key={link} className="flex flex-col items-center gap-1.5 font-mono text-label text-soft-foreground">
            <span className="h-5 w-px bg-border-strong" aria-hidden="true" />
            {link}
            <span className="h-5 w-px bg-border-strong" aria-hidden="true" />
          </span>
        ))}
      </div>

      <div className="rounded-card border border-primary bg-primary-surface p-5 md:p-7">
        <span className="text-title-lg text-primary-ink">gitcask</span>
        <div className="mt-4 flex flex-wrap gap-2">
          {GITCASK.map((item) => (
            <Chip key={item} className="font-sans">
              {item}
            </Chip>
          ))}
        </div>
      </div>

      <div className="mx-auto h-6 w-px bg-border-strong" aria-hidden="true" />
      <div className="flex items-center justify-center gap-2 rounded-card bg-muted px-5 py-4 font-mono text-label text-foreground">
        <BucketGlyph className="size-5 text-primary" />
        S3-compatible bucket
      </div>
    </Figure>
  )
}
