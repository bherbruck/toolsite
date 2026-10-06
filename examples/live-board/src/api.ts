// Every URL here is relative. The app is mounted at /p/<slug>/, so 'api/cards'
// resolves against it; a leading slash would escape to the domain root.

export type Lane = 'todo' | 'doing' | 'done'

export type Card = {
  id: number
  title: string
  lane: Lane
  author_email: string | null
  created_at: number
  updated_at: number
}

export type Person = { id: string; name: string; tabs: number }

export type Me = { id: string; email: string; name: string; conn: string }

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
  if (!res.ok) throw new Error(data?.error ?? `HTTP ${res.status}`)
  return data as T
}

/** The per-app MCP connector, built from the page's own address so it is
 * right on every site the app is deployed to. */
export function connectorUrl(): string {
  return new URL('mcp', document.baseURI).href
}
