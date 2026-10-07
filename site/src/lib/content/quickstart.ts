import 'server-only'

import { readFile } from 'node:fs/promises'
import path from 'node:path'

import { highlighter, THEMES } from './highlighter'

/**
 * The landing's quickstart is README.md's "Try it in five minutes" block, read at build time so
 * the commands have one home. The build fails if that section disappears.
 */
export async function quickstartHtml() {
  const readme = await readFile(path.resolve(/*turbopackIgnore: true*/ process.cwd(), '..', 'README.md'), 'utf8')
  const match = readme.match(/## Try it in five minutes\s+```sh\n([\s\S]*?)```/)
  if (!match) throw new Error('README.md lost its "Try it in five minutes" sh block')

  return highlighter.codeToHtml(match[1].trimEnd(), { lang: 'sh', themes: THEMES, defaultColor: false })
}
