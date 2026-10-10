import 'server-only'

import { highlighter, THEMES } from '@/lib/content/highlighter'
import { cn } from '@/lib/utils/cn'

type Language = 'sh' | 'json' | 'rust' | 'toml' | 'text'

/** Highlighted at build time; `title` names the file or the context the code runs in. */
export function CodeBlock({
  code,
  lang = 'sh',
  title,
  className,
}: {
  code: string
  lang?: Language
  title?: string
  className?: string
}) {
  const html = highlighter.codeToHtml(code.trim(), {
    lang: lang === 'text' ? 'plaintext' : lang,
    themes: THEMES,
    defaultColor: false,
  })

  return (
    <figure className={cn('code-block', className)}>
      {title ? <figcaption className="code-block-title">{title}</figcaption> : null}
      <div dangerouslySetInnerHTML={{ __html: html }} />
    </figure>
  )
}
