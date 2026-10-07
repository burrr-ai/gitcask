import { cn } from '@/lib/utils/cn'

/**
 * Diagram vocabulary for the landing page and the docs. Diagrams are HTML, not scaled SVG, so
 * labels keep their reading size on a phone and reflow instead of shrinking.
 *
 * Tones: `plain` an ordinary step · `brand` the point of the picture (one per diagram) ·
 * `quiet` something outside gitcask · `good` a successful result · `gone` something removed.
 */
export type Tone = 'plain' | 'brand' | 'quiet' | 'good' | 'gone'

const toneClass: Record<Tone, string> = {
  plain: 'border-border-strong/60 bg-card text-foreground',
  brand: 'border-primary bg-primary-surface text-primary-ink font-semibold',
  quiet: 'border-dashed border-border-strong text-soft-foreground',
  good: 'border-success-border bg-success-surface text-success-surface-foreground',
  gone: 'border-dashed border-border-strong text-subtle-foreground line-through decoration-1',
}

export function Chip({
  tone = 'plain',
  code = false,
  className,
  children,
}: {
  tone?: Tone
  /** Set when the label is literal code: a path, a header, a command. */
  code?: boolean
  className?: string
  children: React.ReactNode
}) {
  return (
    <span
      className={cn(
        'inline-flex items-center justify-center gap-2 rounded-control border px-3.5 py-2.5 text-label whitespace-nowrap',
        code && 'font-mono',
        toneClass[tone],
        className
      )}
    >
      {children}
    </span>
  )
}

/** A flow arrow: points down on narrow screens and right from `md` unless `responsive` is off. */
export function Arrow({ className, responsive = true }: { className?: string; responsive?: boolean }) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.6}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      className={cn('size-5 shrink-0 text-subtle-foreground', responsive && 'rotate-90 md:rotate-0', className)}
    >
      <path d="M4 12h15M14 7l5 5-5 5" />
    </svg>
  )
}

/** A chain of chips joined by arrows; stacks vertically below `md`. */
export function Flow({
  steps,
  className,
}: {
  steps: { label: React.ReactNode; tone?: Tone; code?: boolean }[]
  className?: string
}) {
  return (
    <div className={cn('flex flex-col items-start gap-2 md:flex-row md:flex-wrap md:items-center md:gap-3', className)}>
      {steps.map((step, index) => (
        <div key={index} className="flex flex-col items-start gap-2 md:flex-row md:items-center md:gap-3">
          {index > 0 ? <Arrow className="ml-4 md:ml-0" /> : null}
          <Chip tone={step.tone} code={step.code}>
            {step.label}
          </Chip>
        </div>
      ))}
    </div>
  )
}

export function Figure({
  label,
  className,
  children,
}: {
  /** What the diagram shows, for screen readers. */
  label: string
  className?: string
  children: React.ReactNode
}) {
  return (
    <figure className={className}>
      {children}
      <figcaption className="sr-only">{label}</figcaption>
    </figure>
  )
}

export function ServerGlyph({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth={1.5} aria-hidden="true" className={cn('size-4', className)}>
      <rect x="3" y="3.5" width="14" height="5" rx="1.5" />
      <rect x="3" y="11.5" width="14" height="5" rx="1.5" />
      <path d="M6 6h.01M6 14h.01" strokeLinecap="round" strokeWidth={2.2} />
    </svg>
  )
}

export function BucketGlyph({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth={1.5} aria-hidden="true" className={cn('size-4', className)}>
      <ellipse cx="10" cy="5" rx="7" ry="2.5" />
      <path d="M3 5l1.6 10.2c.2 1.3 2.6 2.3 5.4 2.3s5.2-1 5.4-2.3L17 5" />
    </svg>
  )
}
