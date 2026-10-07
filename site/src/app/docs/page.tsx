import type { Metadata } from 'next'

import { DocPage } from '@/lib/components/site/doc-page'
import { DOCS } from '@/lib/content/docs'

const entry = DOCS.find((doc) => doc.slug === '')!

export const metadata: Metadata = { title: entry.label }

export default function DocsIndexPage() {
  return <DocPage entry={entry} />
}
