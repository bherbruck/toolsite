// Every URL here is relative. The app is mounted at /p/<slug>/, so 'api/me'
// resolves against it; a leading slash would escape to the domain root.

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message)
  }
}

export async function api<T = any>(path: string): Promise<T> {
  const res = await fetch(path)
  const text = await res.text()
  let data: any = null
  try {
    data = text ? JSON.parse(text) : null
  } catch {
    data = { error: text }
  }
  if (!res.ok) throw new ApiError(res.status, data?.error ?? `HTTP ${res.status}`)
  return data as T
}

export type ReportFile = { key: string; rows: number; bytes: number; created_at: number }

export function when(seconds: number | null | undefined): string {
  if (!seconds) return 'never'
  return new Date(seconds * 1000).toLocaleString()
}

export function bytes(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}

export function ms(n: number): string {
  return n < 1000 ? `${Math.round(n)} ms` : `${(n / 1000).toFixed(2)} s`
}

export function money(n: number): string {
  if (Math.abs(n) >= 1e6) return `$${(n / 1e6).toFixed(1)}M`
  if (Math.abs(n) >= 1e3) return `$${(n / 1e3).toFixed(0)}K`
  return `$${n.toFixed(0)}`
}
