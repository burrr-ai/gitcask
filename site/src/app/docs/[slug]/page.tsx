import type { Metadata } from 'next'
import { notFound } from 'next/navigation'

import { DocPage } from '@/lib/components/site/doc-page'
import { DOCS, findDocBySlug } from '@/lib/content/docs'

export const dynamicParams = false

export function generateStaticParams() {
  return DOCS.filter((doc) => doc.slug).map((doc) => ({ slug: doc.slug }))
}

export async function generateMetadata({ params }: PageProps<'/docs/[slug]'>): Promise<Metadata> {
  const { slug } = await params
  return { title: findDocBySlug(slug)?.label }
}

export default async function DocsSlugPage({ params }: PageProps<'/docs/[slug]'>) {
  const { slug } = await params
  const entry = findDocBySlug(slug)
  if (!entry) notFound()
  return <DocPage entry={entry} />
}
