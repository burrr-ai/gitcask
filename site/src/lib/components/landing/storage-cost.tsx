'use client'

import Link from 'next/link'
import { useState } from 'react'

import { SegmentedControl } from '@/lib/components/ui/segmented-control'
import { useTween } from './use-tween'

/**
 * Monthly storage at list price for the same bytes in two places:
 *  - hosted git that keeps repositories hot on replicated disks: code.storage, $0.005/GB/hour
 *    ≈ $3.60/GB/month (docs/PRODUCT.md §1, as of 2026-09; egress extra)
 *  - your own bucket: S3 Standard, us-east-1, $0.023/GB/month (first-50-TB rate, applied throughout)
 * Compute, requests and egress are excluded on both sides, and the chart says so.
 * Defaults follow the workload in docs/DIRECTION.md §2.
 */
const HOT_PER_GB = 3.6
const BUCKET_PER_GB = 0.023

const USERS = ['1,000', '10,000', '50,000'] as const
const PROJECTS = ['5', '20', '50'] as const
const SIZES = ['5 MB', '50 MB', '200 MB'] as const

const toNumber = (label: string) => Number(label.replace(/[^0-9]/g, ''))
const dollars = (value: number) =>
  value.toLocaleString('en-US', { style: 'currency', currency: 'USD', maximumFractionDigits: 0 })

export function StorageCost() {
  const [users, setUsers] = useState<(typeof USERS)[number]>('10,000')
  const [projects, setProjects] = useState<(typeof PROJECTS)[number]>('20')
  const [size, setSize] = useState<(typeof SIZES)[number]>('50 MB')

  const repositories = toNumber(users) * toNumber(projects)
  const gigabytes = (repositories * toNumber(size)) / 1000
  const hot = useTween(gigabytes * HOT_PER_GB)
  const bucket = useTween(gigabytes * BUCKET_PER_GB)
  const bucketShare = Math.max(bucket / Math.max(hot, 1), 0)

  return (
    <div className="grid gap-12 lg:grid-cols-[minmax(0,20rem)_1fr] lg:gap-16">
      <div className="space-y-7">
        <Control label="Users">
          <SegmentedControl
            aria-label="Users"
            size="md"
            options={USERS.map((value) => ({ label: value, value }))}
            value={users}
            onValueChange={setUsers}
          />
        </Control>
        <Control label="Projects per user">
          <SegmentedControl
            aria-label="Projects per user"
            size="md"
            options={PROJECTS.map((value) => ({ label: value, value }))}
            value={projects}
            onValueChange={setProjects}
          />
        </Control>
        <Control label="Size per project">
          <SegmentedControl
            aria-label="Size per project"
            size="md"
            options={SIZES.map((value) => ({ label: value, value }))}
            value={size}
            onValueChange={setSize}
          />
        </Control>
        <p className="text-body text-soft-foreground tabular-nums">
          {repositories.toLocaleString('en-US')} repositories,{' '}
          {gigabytes >= 1000 ? `${(gigabytes / 1000).toLocaleString('en-US', { maximumFractionDigits: 1 })} TB` : `${gigabytes.toLocaleString('en-US')} GB`}
        </p>
      </div>

      <figure aria-label="Monthly storage cost at list price" className="min-w-0">
        <p className="text-title-md text-foreground">Monthly storage, list price</p>
        <div className="mt-8 space-y-9">
          <Bar
            title="Hosted git on replicated disks"
            detail="code.storage hot tier, $3.60 per GB"
            amount={dollars(hot)}
            share={1}
            tone="muted"
          />
          <Bar
            title="Your S3 bucket with gitcask"
            detail="S3 Standard, $0.023 per GB"
            amount={dollars(bucket)}
            share={bucketShare}
            tone="brand"
          />
        </div>
        <figcaption className="mt-10 text-label text-soft-foreground">
          Storage only on both sides; compute, requests and egress are not included. Prices from{' '}
          <Link href="/llms/product.md" className="text-primary underline-offset-4 hover:underline">
            the product notes
          </Link>{' '}
          (2026-09) and AWS us-east-1.
        </figcaption>
      </figure>
    </div>
  )
}

function Control({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <p className="mb-2.5 text-title-sm text-foreground">{label}</p>
      {children}
    </div>
  )
}

function Bar({
  title,
  detail,
  amount,
  share,
  tone,
}: {
  title: string
  detail: string
  amount: string
  share: number
  tone: 'muted' | 'brand'
}) {
  return (
    <div>
      <div className="flex flex-wrap items-baseline justify-between gap-x-6 gap-y-1">
        <div>
          <p className="text-title-lg text-foreground">{title}</p>
          <p className="text-body text-soft-foreground">{detail}</p>
        </div>
        <p className="text-display-md text-foreground tabular-nums">{amount}</p>
      </div>
      <div className="mt-4 h-4 overflow-hidden rounded-pill bg-muted">
        <div
          className={tone === 'brand' ? 'h-full rounded-pill bg-primary' : 'h-full rounded-pill bg-border-strong'}
          // A sliver stays visible so the cheap bar never vanishes into the track.
          style={{ width: `max(0.75rem, ${share * 100}%)`, transition: 'width var(--duration-slower) var(--ease-standard, ease)' }}
        />
      </div>
    </div>
  )
}
