import { useEffect, useState } from 'react'

export default function App() {
  const [greeting, setGreeting] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    // Relative on purpose: the app is mounted at /p/unbuilt/, so 'api/hello'
    // resolves against it. A leading slash would escape to the domain root.
    fetch('api/hello')
      .then((res) => (res.ok ? res.text() : Promise.reject(new Error(`HTTP ${res.status}`))))
      .then(setGreeting)
      .catch((e) => setError(String(e)))
  }, [])

  return (
    <main className="mx-auto max-w-2xl px-6 py-16">
      <h1 className="text-3xl font-semibold tracking-tight">unbuilt</h1>
      <p className="mt-2 text-sm text-neutral-500">
        Edit <code className="rounded bg-neutral-500/10 px-1 py-0.5">src/App.tsx</code>, then{' '}
        <code className="rounded bg-neutral-500/10 px-1 py-0.5">npm run build &amp;&amp; toolsite deploy</code>.
      </p>

      <div className="mt-8 rounded-lg border border-neutral-500/20 p-4">
        <div className="text-xs uppercase tracking-wide text-neutral-500">from the handler</div>
        <div className="mt-1 font-mono text-sm">
          {error ? <span className="text-red-500">{error}</span> : (greeting ?? 'loading…')}
        </div>
      </div>
    </main>
  )
}
