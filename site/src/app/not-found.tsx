import Link from 'next/link'

import { Button } from '@/lib/components/ui/button'

export default function NotFound() {
  return (
    <main className="mx-auto max-w-content px-gutter py-32">
      <h1 className="text-statement text-foreground">Nothing at this path.</h1>
      <Button asChild className="mt-10" size="lg">
        <Link href="/">Back to gitcask</Link>
      </Button>
    </main>
  )
}
