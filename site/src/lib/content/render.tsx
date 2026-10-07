import 'server-only'

import { readFile } from 'node:fs/promises'
import path from 'node:path'
import type { ComponentProps } from 'react'
import { Fragment, jsx, jsxs } from 'react/jsx-runtime'
import Link from 'next/link'
import type { Element, ElementContent, Root } from 'hast'
import { toJsxRuntime } from 'hast-util-to-jsx-runtime'
import rehypeSlug from 'rehype-slug'
import remarkGfm from 'remark-gfm'
import remarkParse from 'remark-parse'
import remarkRehype from 'remark-rehype'
import { unified } from 'unified'

import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/lib/components/ui/table'
import { docHref, findDocBySource, REPOSITORY_URL, type DocEntry } from './docs'
import { highlighter, THEMES } from './highlighter'

/** The site lives in `site/`; the Markdown it renders lives one level up. Read at build time only. */
const REPOSITORY_ROOT = path.resolve(/*turbopackIgnore: true*/ process.cwd(), '..')

export type Heading = { id: string; text: string }

export type RenderedDoc = {
  title: string
  headings: Heading[]
  body: React.ReactNode
}

const processor = unified()
  .use(remarkParse)
  .use(remarkGfm)
  .use(remarkRehype)
  .use(rehypeSlug)

export async function renderDoc(entry: DocEntry): Promise<RenderedDoc> {
  const markdown = await readFile(path.join(/*turbopackIgnore: true*/ REPOSITORY_ROOT, entry.source), 'utf8')
  const tree = (await processor.run(processor.parse(markdown))) as Root

  rewriteLinks(tree, entry.source)
  highlightCode(tree)
  const title = takeTitle(tree) ?? entry.label
  const headings = collectHeadings(tree)

  const body = toJsxRuntime(tree, {
    Fragment,
    jsx,
    jsxs,
    components: {
      a: DocLink,
      table: (props: ComponentProps<'table'>) => <Table className="text-body" {...props} />,
      thead: (props: ComponentProps<'thead'>) => <TableHeader {...props} />,
      tbody: (props: ComponentProps<'tbody'>) => <TableBody {...props} />,
      tr: (props: ComponentProps<'tr'>) => <TableRow {...props} />,
      th: (props: ComponentProps<'th'>) => (
        <TableHead className="align-bottom text-label text-foreground" {...props} />
      ),
      td: (props: ComponentProps<'td'>) => <TableCell className="whitespace-normal align-top" {...props} />,
    },
  })

  return { title, headings, body }
}

function DocLink({ href = '', ...props }: ComponentProps<'a'>) {
  if (href.startsWith('/')) return <Link href={href} {...props} />
  if (href.startsWith('#')) return <a href={href} {...props} />
  return <a href={href} rel="noreferrer" {...props} />
}

/**
 * Relative links in the Markdown point at repository files. A file the site renders becomes its
 * page; every other path (config, crates, the Korean README) opens on GitHub.
 */
function rewriteLinks(tree: Root, source: string) {
  visitElements(tree, (node) => {
    if (node.tagName !== 'a') return
    const href = node.properties.href
    if (typeof href !== 'string' || /^([a-z]+:|#)/i.test(href)) return

    const [target, hash] = href.split('#')
    const resolved = path.posix.normalize(path.posix.join(path.posix.dirname(source), target))
    const anchor = hash ? `#${hash}` : ''
    const doc = findDocBySource(resolved)

    if (doc) {
      node.properties.href = `${docHref(doc)}${anchor}`
    } else {
      const kind = path.posix.extname(resolved) ? 'blob' : 'tree'
      node.properties.href = `${REPOSITORY_URL}/${kind}/main/${resolved}${anchor}`
    }
  })
}

/** Fenced blocks in a language the highlighter loaded get shiki's dual-theme markup. */
function highlightCode(tree: Root) {
  const loaded = new Set(highlighter.getLoadedLanguages())
  visitParents(tree, (node, parent, index) => {
    if (node.tagName !== 'pre') return
    const code = node.children[0]
    if (code?.type !== 'element' || code.tagName !== 'code') return
    const language = [code.properties.className]
      .flat()
      .map(String)
      .find((name) => name.startsWith('language-'))
      ?.slice('language-'.length)
    if (!language || !loaded.has(language)) return

    const highlighted = highlighter.codeToHast(textOf(code).trimEnd(), {
      lang: language,
      themes: THEMES,
      defaultColor: false,
    })
    parent.children.splice(index, 1, ...(highlighted.children as ElementContent[]))
  })
}

/** The first `# heading` becomes the page title rendered by the layout, not part of the body. */
function takeTitle(tree: Root) {
  const index = tree.children.findIndex(
    (node) => node.type === 'element' && node.tagName === 'h1'
  )
  if (index === -1) return undefined
  const [h1] = tree.children.splice(index, 1) as Element[]
  return textOf(h1)
}

function collectHeadings(tree: Root): Heading[] {
  const headings: Heading[] = []
  visitElements(tree, (node) => {
    if (node.tagName === 'h2' && typeof node.properties.id === 'string') {
      headings.push({ id: node.properties.id, text: textOf(node) })
    }
  })
  return headings
}

function visitParents(
  node: Root | Element,
  visit: (element: Element, parent: Root | Element, index: number) => void
) {
  node.children.forEach((child, index) => {
    if (child.type !== 'element') return
    visit(child, node, index)
    visitParents(child, visit)
  })
}

function visitElements(node: Root | ElementContent, visit: (element: Element) => void) {
  if (node.type === 'element') visit(node)
  if ('children' in node) {
    for (const child of node.children) visitElements(child as ElementContent, visit)
  }
}

function textOf(node: ElementContent): string {
  if (node.type === 'text') return node.value
  if ('children' in node) return node.children.map((child) => textOf(child)).join('')
  return ''
}
