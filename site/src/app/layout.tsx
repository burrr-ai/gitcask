import type { Metadata, Viewport } from 'next'
import localFont from 'next/font/local'
import { Noto_Sans_Mono } from 'next/font/google'

import { ThemeProvider } from '@/lib/components/site/theme-provider'
import { SiteHeader } from '@/lib/components/site/site-header'
import './globals.css'

const pretendard = localFont({
  src: './fonts/PretendardStdVariable.woff2',
  display: 'swap',
  weight: '45 920',
  variable: '--font-pretendard',
})

const notoSansMono = Noto_Sans_Mono({
  variable: '--font-noto-sans-mono',
  subsets: ['latin'],
})

const description =
  'Open-source git hosting that keeps every repository in your S3 bucket. No database, no leader; cost follows pushes, not repositories.'

export const metadata: Metadata = {
  title: { default: 'gitcask — open-source git hosting on your S3 bucket', template: '%s · gitcask' },
  description,
  openGraph: { type: 'website', siteName: 'gitcask', title: 'gitcask', description },
  twitter: { card: 'summary', title: 'gitcask', description },
}

export const viewport: Viewport = {
  width: 'device-width',
  initialScale: 1,
  themeColor: [
    { media: '(prefers-color-scheme: light)', color: '#ffffff' },
    { media: '(prefers-color-scheme: dark)', color: '#161618' },
  ],
}

export default function RootLayout({ children }: Readonly<{ children: React.ReactNode }>) {
  return (
    <html lang="en" suppressHydrationWarning>
      <body className={`${pretendard.variable} ${notoSansMono.variable} antialiased`}>
        <ThemeProvider>
          <SiteHeader />
          {children}
        </ThemeProvider>
      </body>
    </html>
  )
}
