/**
 * gitcask mark — a cask seen from the side: two hoops hold the staves, and the middle stave is a
 * commit line. Drawn in currentColor so it follows the brand token in both themes.
 */
export function BrandMark({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 24 24" fill="none" aria-hidden="true" className={className}>
      <path
        d="M6.5 3h11c1.6 2.6 2.4 5.6 2.4 9s-.8 6.4-2.4 9h-11C4.9 18.4 4.1 15.4 4.1 12S4.9 5.6 6.5 3Z"
        fill="currentColor"
        opacity="0.16"
      />
      <path
        d="M6.5 3h11c1.6 2.6 2.4 5.6 2.4 9s-.8 6.4-2.4 9h-11C4.9 18.4 4.1 15.4 4.1 12S4.9 5.6 6.5 3Z"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinejoin="round"
      />
      <path d="M5 7.5h14M5 16.5h14" stroke="currentColor" strokeWidth="1.6" />
      <path d="M12 3v18" stroke="currentColor" strokeWidth="1.6" />
      <circle cx="12" cy="12" r="2.1" fill="var(--background)" stroke="currentColor" strokeWidth="1.6" />
    </svg>
  )
}
