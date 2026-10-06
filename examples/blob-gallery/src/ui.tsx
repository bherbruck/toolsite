import { useCallback, useEffect, useState, type ReactNode } from 'react'

/** Loads once, and again on reload(). */
export function useLoad<T>(load: () => Promise<T>, deps: unknown[] = []) {
  const [data, setData] = useState<T | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [tick, setTick] = useState(0)
  useEffect(() => {
    let live = true
    load()
      .then((d) => live && (setData(d), setError(null)))
      .catch((e) => live && setError(String(e.message ?? e)))
    return () => {
      live = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tick, ...deps])
  const reload = useCallback(() => setTick((t) => t + 1), [])
  return { data, error, reload }
}

export function Screen({ title, lead, children }: { title: string; lead: ReactNode; children: ReactNode }) {
  return (
    <div className="mx-auto max-w-4xl">
      <h1 className="text-2xl font-semibold tracking-tight">{title}</h1>
      <p className="mt-1 max-w-2xl text-sm text-neutral-500">{lead}</p>
      <div className="mt-6 space-y-6">{children}</div>
    </div>
  )
}

export function Card({ title, children, actions }: { title?: string; children: ReactNode; actions?: ReactNode }) {
  return (
    <section className="rounded-xl border border-neutral-500/20 bg-white/60 p-4 shadow-sm dark:bg-neutral-900/60">
      {(title || actions) && (
        <div className="mb-3 flex items-center justify-between gap-2">
          {title && <h2 className="text-sm font-semibold uppercase tracking-wide text-neutral-500">{title}</h2>}
          {actions}
        </div>
      )}
      {children}
    </section>
  )
}

export function Table({ rows, columns }: { rows: Record<string, unknown>[]; columns?: string[] }) {
  const cols = columns ?? (rows[0] ? Object.keys(rows[0]) : [])
  if (rows.length === 0) return <p className="text-sm text-neutral-500">No rows.</p>
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-left text-sm">
        <thead>
          <tr className="border-b border-neutral-500/20 text-xs uppercase tracking-wide text-neutral-500">
            {cols.map((c) => (
              <th key={c} className="px-2 py-1.5 font-medium">
                {c}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={i} className="border-b border-neutral-500/10 last:border-0">
              {cols.map((c) => (
                <td key={c} className="px-2 py-1.5 font-mono text-xs">
                  {r[c] === null || r[c] === undefined ? <span className="text-neutral-400">null</span> : String(r[c])}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

export function ErrorText({ error }: { error: string | null }) {
  if (!error) return null
  return <p className="rounded-md bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-400">{error}</p>
}

export function Button({
  children,
  onClick,
  kind = 'primary',
  type = 'button',
  disabled,
}: {
  children: ReactNode
  onClick?: () => void
  kind?: 'primary' | 'quiet'
  type?: 'button' | 'submit'
  disabled?: boolean
}) {
  const look =
    kind === 'primary'
      ? 'bg-indigo-600 text-white hover:bg-indigo-500'
      : 'border border-neutral-500/30 hover:bg-neutral-500/10'
  return (
    <button
      type={type}
      onClick={onClick}
      disabled={disabled}
      className={`rounded-md px-3 py-1.5 text-sm font-medium disabled:opacity-50 ${look}`}
    >
      {children}
    </button>
  )
}

export const input =
  'rounded-md border border-neutral-500/30 bg-transparent px-2.5 py-1.5 text-sm outline-none focus:border-indigo-500'

export function Code({ children }: { children: ReactNode }) {
  return <code className="rounded bg-neutral-500/10 px-1 py-0.5 font-mono text-[0.85em]">{children}</code>
}

export function CopyField({ value }: { value: string }) {
  const [copied, setCopied] = useState(false)
  return (
    <div className="flex items-center gap-2">
      <input readOnly value={value} className={`${input} flex-1 font-mono`} onFocus={(e) => e.target.select()} />
      <Button
        onClick={() => {
          navigator.clipboard?.writeText(value).then(() => {
            setCopied(true)
            setTimeout(() => setCopied(false), 1500)
          })
        }}
      >
        {copied ? 'Copied' : 'Copy'}
      </Button>
    </div>
  )
}
