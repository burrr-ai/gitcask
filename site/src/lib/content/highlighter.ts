import 'server-only'

import { createHighlighterCore } from 'shiki/core'
import { createJavaScriptRegexEngine } from 'shiki/engine/javascript'

/**
 * Only the grammars the Markdown uses (`sh`, `json`, `rust`, `toml`) and two themes. The full
 * shiki bundle tripled the Worker; every page is prerendered, so this runs at build time anyway.
 */
export const THEMES = { light: 'github-light', dark: 'github-dark' } as const

export const highlighter = await createHighlighterCore({
  themes: [import('shiki/dist/themes/github-light.mjs'), import('shiki/dist/themes/github-dark.mjs')],
  langs: [
    import('shiki/dist/langs/shellscript.mjs'),
    import('shiki/dist/langs/json.mjs'),
    import('shiki/dist/langs/rust.mjs'),
    import('shiki/dist/langs/toml.mjs'),
  ],
  engine: createJavaScriptRegexEngine(),
})
