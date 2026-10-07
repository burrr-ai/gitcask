import type { NextConfig } from 'next'
import { initOpenNextCloudflareForDev } from '@opennextjs/cloudflare'

const nextConfig: NextConfig = {
  reactStrictMode: true,
  // Next externalizes shiki by default, which makes the Worker carry every grammar and theme.
  // Bundling it keeps only the four grammars and two themes the highlighter imports.
  transpilePackages: ['shiki'],
}

export default nextConfig

initOpenNextCloudflareForDev()
