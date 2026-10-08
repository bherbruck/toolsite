// Two single-series charts drawn in SVG: a line over months and horizontal
// bars by store. One series each, so the title names it and no legend is
// needed; every mark has a hover tooltip, and the numbers are in a table
// beside them.

import { useState } from 'react'
import { money } from './api'

const H = 220
const PAD = { top: 12, right: 12, bottom: 28, left: 56 }

function ticks(max: number, count = 4): number[] {
  if (max <= 0) return [0]
  const raw = max / count
  const step = 10 ** Math.floor(Math.log10(raw))
  const nice = [1, 2, 5, 10].map((m) => m * step).find((s) => s >= raw) ?? raw
  const out = []
  for (let v = 0; v <= max + nice / 2; v += nice) out.push(v)
  return out
}

export function LineChart({ points }: { points: { label: string; value: number }[] }) {
  const [hover, setHover] = useState<number | null>(null)
  const W = 640
  const innerW = W - PAD.left - PAD.right
  const innerH = H - PAD.top - PAD.bottom
  const ys = ticks(Math.max(...points.map((p) => p.value), 0))
  const top = ys[ys.length - 1] || 1
  const x = (i: number) => PAD.left + (points.length <= 1 ? innerW / 2 : (i * innerW) / (points.length - 1))
  const y = (v: number) => PAD.top + innerH - (v / top) * innerH
  const path = points.map((p, i) => `${i ? 'L' : 'M'}${x(i)},${y(p.value)}`).join('')
  const step = Math.max(1, Math.ceil(points.length / 8))
  return (
    <div className="relative">
      <svg viewBox={`0 0 ${W} ${H}`} className="w-full" role="img" aria-label="Revenue by month"
        onMouseLeave={() => setHover(null)}
        onMouseMove={(e) => {
          const box = e.currentTarget.getBoundingClientRect()
          const px = ((e.clientX - box.left) / box.width) * W
          const i = Math.round(((px - PAD.left) / innerW) * (points.length - 1))
          setHover(i >= 0 && i < points.length ? i : null)
        }}>
        {ys.map((v) => (
          <g key={v}>
            <line x1={PAD.left} x2={W - PAD.right} y1={y(v)} y2={y(v)} className="stroke-[var(--grid)]" />
            <text x={PAD.left - 8} y={y(v)} dy="0.32em" textAnchor="end" className="fill-[var(--muted)] text-[11px]">{money(v)}</text>
          </g>
        ))}
        {points.map((p, i) => i % step === 0 && (
          <text key={p.label} x={x(i)} y={H - 8} textAnchor="middle" className="fill-[var(--muted)] text-[11px]">{p.label}</text>
        ))}
        <path d={path} fill="none" strokeWidth={2} className="stroke-[var(--series-1)]" />
        {hover !== null && (
          <g>
            <line x1={x(hover)} x2={x(hover)} y1={PAD.top} y2={PAD.top + innerH} className="stroke-[var(--muted)]" strokeDasharray="3 3" />
            <circle cx={x(hover)} cy={y(points[hover].value)} r={4} strokeWidth={2} className="fill-[var(--series-1)] stroke-[var(--surface)]" />
          </g>
        )}
      </svg>
      {hover !== null && (
        <div className="pointer-events-none absolute top-1 rounded-md border border-neutral-500/20 bg-[var(--surface)] px-2 py-1 text-xs shadow"
          style={{ left: `${(x(hover) / W) * 100}%`, transform: 'translateX(-50%)' }}>
          <div className="text-[var(--muted)]">{points[hover].label}</div>
          <div className="font-medium tabular-nums">{money(points[hover].value)}</div>
        </div>
      )}
    </div>
  )
}

export function BarChart({ bars }: { bars: { label: string; value: number }[] }) {
  const [hover, setHover] = useState<number | null>(null)
  const max = Math.max(...bars.map((b) => b.value), 1)
  return (
    <div className="space-y-[2px]" role="img" aria-label="Revenue by store">
      {bars.map((b, i) => (
        <div key={b.label} className="flex items-center gap-2 py-0.5 text-xs" onMouseEnter={() => setHover(i)} onMouseLeave={() => setHover(null)}>
          <span className="w-16 shrink-0 text-[var(--muted)]">{b.label}</span>
          <div className="relative h-3 flex-1">
            <div className="h-3 rounded-r bg-[var(--series-1)]" style={{ width: `${(b.value / max) * 100}%`, opacity: hover === null || hover === i ? 1 : 0.5 }} />
          </div>
          <span className={`w-14 shrink-0 text-right tabular-nums ${hover === i ? 'font-medium' : 'text-[var(--muted)]'}`}>{money(b.value)}</span>
        </div>
      ))}
    </div>
  )
}
