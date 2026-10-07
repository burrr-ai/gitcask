'use client'

import { useEffect, useRef, useState } from 'react'

/**
 * The hero's proof: every square is a repository. Idle squares are dim and cost only their bytes in
 * the bucket; a push lights one up, and that is the only work gitcask does for it. Moving the pointer
 * (or tapping) pushes to the squares underneath.
 *
 * Drawing: the whole field is painted once; each frame repaints only the squares that are cooling.
 */

const PITCH = 9 // px between square origins
const SIZE = 6 // square side
const RADIUS = 1.5
const COOL_MS = 3200
const PUSHES_PER_SECOND = 16

type Lit = { index: number; born: number }

function readColors() {
  const style = getComputedStyle(document.documentElement)
  return {
    idle: style.getPropertyValue('--fill-strong').trim() || '#dfe2e7',
    hot: style.getPropertyValue('--brand').trim() || '#a65310',
    background: style.getPropertyValue('--background').trim() || '#ffffff',
  }
}

export type PushSample = { x: number; y: number; id: number }

export function RepositoryField({
  onPush,
  onSample,
}: {
  onPush?: (count: number) => void
  /** Every few seconds, one push in the open (right-hand) part of the field, in CSS pixels. */
  onSample?: (sample: PushSample) => void
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const onPushRef = useRef(onPush)
  const onSampleRef = useRef(onSample)
  const [cells, setCells] = useState(0)

  useEffect(() => {
    onPushRef.current = onPush
    onSampleRef.current = onSample
  }, [onPush, onSample])

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    const context = canvas.getContext('2d')
    if (!context) return

    const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches
    let colors = readColors()
    let columns = 0
    let rows = 0
    let width = 0
    let height = 0
    let lit = new Map<number, Lit>()
    let frame = 0
    let lastSpawn = performance.now()
    let reported = -1
    let lastSample = performance.now()
    let samples = 0
    const recent: number[] = []

    const position = (index: number) => [(index % columns) * PITCH + 1, Math.floor(index / columns) * PITCH + 1] as const

    function paint(index: number, heat: number) {
      const [x, y] = position(index)
      context!.clearRect(x - 1, y - 1, SIZE + 2, SIZE + 2)
      context!.globalAlpha = 1
      context!.fillStyle = colors.idle
      context!.beginPath()
      context!.roundRect(x, y, SIZE, SIZE, RADIUS)
      context!.fill()
      if (heat > 0) {
        context!.globalAlpha = heat * 0.18
        context!.fillStyle = colors.hot
        context!.beginPath()
        context!.roundRect(x - 1, y - 1, SIZE + 2, SIZE + 2, RADIUS + 1)
        context!.fill()
        context!.globalAlpha = heat
        context!.fillStyle = colors.hot
        context!.beginPath()
        context!.roundRect(x, y, SIZE, SIZE, RADIUS)
        context!.fill()
        context!.globalAlpha = 1
      }
    }

    function paintAll() {
      context!.clearRect(0, 0, width, height)
      for (let index = 0; index < columns * rows; index++) paint(index, 0)
      for (const cell of lit.values()) paint(cell.index, 1)
    }

    function resize() {
      const rect = canvas!.getBoundingClientRect()
      const ratio = Math.min(window.devicePixelRatio || 1, 2)
      width = rect.width
      height = rect.height
      canvas!.width = Math.round(width * ratio)
      canvas!.height = Math.round(height * ratio)
      context!.setTransform(ratio, 0, 0, ratio, 0, 0)
      columns = Math.max(1, Math.floor(width / PITCH))
      rows = Math.max(1, Math.floor(height / PITCH))
      lit = new Map()
      setCells(columns * rows)
      if (reduceMotion) {
        // A still frame: about one square in a hundred mid-push.
        for (let n = 0; n < (columns * rows) / 100; n++) {
          const index = Math.floor(Math.random() * columns * rows)
          lit.set(index, { index, born: 0 })
        }
      }
      paintAll()
    }

    function push(index: number, now: number) {
      if (index < 0 || index >= columns * rows) return
      lit.set(index, { index, born: now })
      recent.push(now)
    }

    function tick(now: number) {
      const interval = 1000 / PUSHES_PER_SECOND
      while (now - lastSpawn > interval) {
        lastSpawn += interval * (0.4 + Math.random() * 1.2)
        push(Math.floor(Math.random() * columns * rows), now)
      }
      for (const cell of lit.values()) {
        const heat = 1 - (now - cell.born) / COOL_MS
        if (heat <= 0) {
          lit.delete(cell.index)
          paint(cell.index, 0)
        } else {
          paint(cell.index, heat * heat)
        }
      }
      if (now - lastSample > 2400 && columns > 8) {
        lastSample = now
        const wide = width >= 768
        const column = Math.floor(columns * (wide ? 0.58 + Math.random() * 0.34 : 0.15 + Math.random() * 0.6))
        const row = Math.floor(rows * (wide ? 0.15 + Math.random() * 0.6 : 0.62 + Math.random() * 0.15))
        const index = row * columns + column
        push(index, now)
        const [x, y] = position(index)
        onSampleRef.current?.({ x: x + SIZE / 2, y: y + SIZE / 2, id: ++samples })
      }
      while (recent.length && now - recent[0] > 5000) recent.shift()
      if (recent.length !== reported) {
        reported = recent.length
        onPushRef.current?.(reported)
      }
      frame = requestAnimationFrame(tick)
    }

    let lastPointer = 0
    function pointer(event: PointerEvent, burst: boolean) {
      if (reduceMotion) return
      const now = performance.now()
      if (!burst && now - lastPointer < 45) return
      lastPointer = now
      const rect = canvas!.getBoundingClientRect()
      const column = Math.floor((event.clientX - rect.left) / PITCH)
      const row = Math.floor((event.clientY - rect.top) / PITCH)
      const reach = burst ? 3 : 1
      for (let dy = -reach; dy <= reach; dy++) {
        for (let dx = -reach; dx <= reach; dx++) {
          if (dx * dx + dy * dy > reach * reach) continue
          if (!burst && Math.random() < 0.55) continue
          const c = column + dx
          const r = row + dy
          if (c >= 0 && c < columns && r >= 0 && r < rows) push(r * columns + c, now)
        }
      }
    }
    const move = (event: PointerEvent) => pointer(event, false)
    const down = (event: PointerEvent) => pointer(event, true)

    const observer = new ResizeObserver(resize)
    observer.observe(canvas)
    const themeObserver = new MutationObserver(() => {
      colors = readColors()
      paintAll()
    })
    themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ['class', 'style'] })
    canvas.addEventListener('pointermove', move)
    canvas.addEventListener('pointerdown', down)
    if (!reduceMotion) frame = requestAnimationFrame(tick)

    return () => {
      cancelAnimationFrame(frame)
      observer.disconnect()
      themeObserver.disconnect()
      canvas.removeEventListener('pointermove', move)
      canvas.removeEventListener('pointerdown', down)
    }
  }, [])

  return (
    <canvas
      ref={canvasRef}
      role="img"
      aria-label={`A field of ${cells.toLocaleString('en-US')} repositories. Most are idle; the few being pushed to light up and cool down.`}
      className="absolute inset-0 size-full touch-pan-y"
    />
  )
}
