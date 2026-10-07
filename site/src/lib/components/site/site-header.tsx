import Link from 'next/link'

import { Button } from '@/lib/components/ui/button'
import { REPOSITORY_URL } from '@/lib/content/docs'
import { BrandMark } from './brand-mark'
import { ThemeToggle } from './theme-toggle'

export function SiteHeader() {
  return (
    <header className="sticky top-0 z-appbar border-b border-border bg-background/85 backdrop-blur-md">
      <div className="mx-auto flex h-appbar max-w-content items-center gap-2 px-gutter">
        <Link href="/" className="mr-auto flex items-center gap-2 text-title-lg text-foreground">
          <BrandMark className="size-7 text-primary" />
          gitcask
        </Link>
        <Button asChild variant="ghost">
          <Link href="/docs">Docs</Link>
        </Button>
        <Button asChild variant="ghost">
          <a href={REPOSITORY_URL} rel="noreferrer">
            GitHub
          </a>
        </Button>
        <ThemeToggle />
      </div>
    </header>
  )
}
