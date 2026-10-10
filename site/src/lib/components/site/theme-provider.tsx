'use client'

import { ThemeProvider as NextThemesProvider } from 'next-themes'

/** The tokens switch on `.dark` (comwit-tokens.css), so the theme is a class on <html>. */
export function ThemeProvider({ children }: { children: React.ReactNode }) {
  return (
    <NextThemesProvider attribute="class" defaultTheme="system" enableSystem disableTransitionOnChange>
      {children}
    </NextThemesProvider>
  )
}
