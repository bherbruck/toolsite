// Every URL here is relative. The app is mounted at /p/<slug>/, so 'api/me'
// resolves against it; a leading slash would escape to the domain root.

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message)
  }
}

export async function api<T = any>(path: string, init?: { method?: string; body?: unknown }): Promise<T> {
  const res = await fetch(path, {
    method: init?.method ?? (init?.body === undefined ? 'GET' : 'POST'),
    headers: init?.body === undefined ? undefined : { 'content-type': 'application/json' },
    body: init?.body === undefined ? undefined : JSON.stringify(init.body),
  })
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

/** The per-app MCP connector, built from the page's own address so it is
 * right on every site the app is deployed to. */
export function connectorUrl(): string {
  return new URL('mcp', document.baseURI).href
}

export function when(seconds: number | null | undefined): string {
  if (!seconds) return 'never'
  return new Date(seconds * 1000).toLocaleString()
}

export function bytes(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`
  return `${(n / 1024 / 1024).toFixed(1)} MB`
}
