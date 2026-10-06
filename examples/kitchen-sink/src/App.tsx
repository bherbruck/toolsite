import { useEffect, useRef, useState } from 'react'
import { api } from './api'
import { Admin, Connect, Database, Fetch, Files, Identity, Jobs, Orders, Routes, Settings, SqlConsole, type Me } from './screens'
import { useLoad } from './ui'

// One entry per capability. The URL hash picks the screen, so a reload or a
// shared link lands on the same one without any server-side routing.
const SCREENS = [
  { id: 'database', label: 'Database', hint: 'SQL and migrations' },
  { id: 'orders', label: 'Orders', hint: 'Row-level access' },
  { id: 'sql', label: 'SQL as you', hint: 'query-scoped' },
  { id: 'files', label: 'Files', hint: 'Blob storage' },
  { id: 'settings', label: 'Settings', hint: 'Secrets' },
  { id: 'fetch', label: 'Fetch', hint: 'Outbound HTTP' },
  { id: 'identity', label: 'Identity', hint: 'User and roles' },
  { id: 'jobs', label: 'Jobs', hint: 'Schedule' },
  { id: 'routes', label: 'Routes', hint: 'Per-path access' },
  { id: 'admin', label: 'Admin', hint: 'Managers only' },
  { id: 'connect', label: 'AI tools', hint: 'MCP connector' },
] as const

type ScreenId = (typeof SCREENS)[number]['id']

function current(): ScreenId {
  const id = location.hash.replace(/^#\/?/, '')
  return (SCREENS.find((s) => s.id === id)?.id ?? 'database') as ScreenId
}

export default function App() {
  const [screen, setScreen] = useState<ScreenId>(current)
  const [menu, setMenu] = useState(false)
  const menuRef = useRef<HTMLDivElement>(null)
  const me = useLoad<Me>(() => api('api/me'))

  useEffect(() => {
    const onHash = () => setScreen(current())
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])
  useEffect(() => {
    const close = (e: MouseEvent) => menuRef.current && !menuRef.current.contains(e.target as Node) && setMenu(false)
    document.addEventListener('mousedown', close)
    return () => document.removeEventListener('mousedown', close)
  }, [])

  const go = (id: ScreenId) => {
    location.hash = `/${id}`
    setMenu(false)
  }

  return (
    <div className="flex min-h-screen bg-neutral-50 text-neutral-900 dark:bg-neutral-950 dark:text-neutral-100">
      <aside className="hidden w-56 shrink-0 border-r border-neutral-500/15 p-3 md:block">
        <div className="mb-4 flex items-center gap-2 px-2 pt-1">
          <span className="text-xl">🧰</span>
          <span className="font-semibold">Kitchen sink</span>
        </div>
        <nav className="space-y-0.5">
          {SCREENS.map((s) => (
            <button
              key={s.id}
              onClick={() => go(s.id)}
              className={`block w-full rounded-md px-2 py-1.5 text-left text-sm ${
                screen === s.id ? 'bg-indigo-600 text-white' : 'hover:bg-neutral-500/10'
              }`}
            >
              <div className="font-medium">{s.label}</div>
              <div className={`text-xs ${screen === s.id ? 'text-indigo-100' : 'text-neutral-500'}`}>{s.hint}</div>
            </button>
          ))}
        </nav>
      </aside>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex items-center justify-between gap-3 border-b border-neutral-500/15 px-6 py-3">
          <select className="rounded-md border border-neutral-500/30 bg-transparent px-2 py-1 text-sm md:hidden" value={screen} onChange={(e) => go(e.target.value as ScreenId)}>
            {SCREENS.map((s) => (
              <option key={s.id} value={s.id}>
                {s.label}
              </option>
            ))}
          </select>
          <div className="hidden text-sm text-neutral-500 md:block">Everything toolsite can do, one screen each.</div>
          <div className="flex items-center gap-3">
            <span className="text-sm text-neutral-500">
              {me.data?.user?.email ?? 'not signed in'}
              {me.data?.role && <span className="ml-2 rounded bg-neutral-500/10 px-1.5 py-0.5 text-xs">{me.data.role}</span>}
            </span>
            <div className="relative" ref={menuRef}>
              <button onClick={() => setMenu(!menu)} className="rounded-md px-2 py-1 text-lg leading-none hover:bg-neutral-500/10" aria-label="Menu">
                ⋯
              </button>
              {menu && (
                <div className="absolute right-0 z-10 mt-1 w-60 rounded-lg border border-neutral-500/20 bg-white p-1 shadow-lg dark:bg-neutral-900">
                  <button onClick={() => go('connect')} className="block w-full rounded-md px-3 py-2 text-left text-sm hover:bg-neutral-500/10">
                    Connect an AI assistant
                  </button>
                  <a href="status" target="_blank" rel="noreferrer" className="block rounded-md px-3 py-2 text-sm hover:bg-neutral-500/10">
                    Status
                  </a>
                </div>
              )}
            </div>
          </div>
        </header>

        <main className="flex-1 px-6 py-8">
          {screen === 'database' && <Database />}
          {screen === 'orders' && <Orders me={me.data} />}
          {screen === 'sql' && <SqlConsole />}
          {screen === 'files' && <Files />}
          {screen === 'settings' && <Settings />}
          {screen === 'fetch' && <Fetch />}
          {screen === 'identity' && <Identity me={me.data} />}
          {screen === 'jobs' && <Jobs />}
          {screen === 'routes' && <Routes />}
          {screen === 'admin' && <Admin me={me.data} />}
          {screen === 'connect' && <Connect />}
        </main>
      </div>
    </div>
  )
}
