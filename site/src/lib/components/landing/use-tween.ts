'use client'

import { useEffect, useRef, useState } from 'react'

/** Eases a displayed number toward `target`; jumps straight there when motion is reduced. */
export function useTween(target: number, duration = 700) {
  const [value, setValue] = useState(target)
  const from = useRef(target)

  useEffect(() => {
    if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) {
      from.current = target
      setValue(target)
      return
    }
    const start = performance.now()
    const origin = from.current
    let frame = 0
    const step = (now: number) => {
      const t = Math.min(1, (now - start) / duration)
      const eased = 1 - Math.pow(1 - t, 4)
      const next = origin + (target - origin) * eased
      from.current = next
      setValue(next)
      if (t < 1) frame = requestAnimationFrame(step)
    }
    frame = requestAnimationFrame(step)
    return () => cancelAnimationFrame(frame)
  }, [target, duration])

  return value
}
