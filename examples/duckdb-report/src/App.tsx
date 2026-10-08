import { useEffect, useState, type ReactNode } from 'react'
import { api, bytes, money, ms, when, type ReportFile } from './api'
import { BarChart, LineChart } from './charts'
import { loadFile, query, type Loaded, type Rows } from './duck'

// The name the file is registered under inside DuckDB. Queries read it as a
// local file; it is never a URL.
const LOCAL_NAME = 'sales.parquet'

type Results = {
  monthly: Rows
  stores: Rows
  products: Rows
  totals: Rows
  ms: number
}

export default function App() {
  const [file, setFile] = useState<ReportFile | null>(null)
  const [loaded, setLoaded] = useState<Loaded | null>(null)
  const [regions, setRegions] = useState<string[]>([])
  const [region, setRegion] = useState('')
  const [results, setResults] = useState<Results | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState('Finding the latest report…')

  // The handler lists what the job wrote, and refuses without a grant.
  useEffect(() => {
    api<{ files: ReportFile[] }>('api/files')
      .then(async ({ files }) => {
        const latest = files[0]
        if (!latest) {
          setBusy('')
          return
        }
        setFile(latest)
        setBusy(`Downloading ${bytes(latest.bytes)}…`)
        // The whole file through the handler, then into DuckDB as bytes.
        const got = await loadFile(`api/files/${latest.key}`, LOCAL_NAME)
        setLoaded(got)
        const { rows } = await query(`select distinct region from '${LOCAL_NAME}' order by region`)
        setRegions(rows.map((r) => String(r.region)))
        setBusy('')
      })
      .catch((e) => {
        setError(String(e.message ?? e))
        setBusy('')
      })
  }, [])

  useEffect(() => {
    if (!loaded) return
    const where = region ? 'where region = ?' : ''
    const params = region ? [region] : []
    const run = async () => {
      const started = performance.now()
      const [monthly, stores, products, totals] = await Promise.all([
        query(`select strftime(date_trunc('month', day), '%Y-%m') as month, sum(revenue) as revenue
               from '${LOCAL_NAME}' ${where} group by 1 order by 1`, params),
        query(`select store, sum(revenue) as revenue from '${LOCAL_NAME}' ${where} group by 1 order by 2 desc`, params),
        query(`select product, sum(units) as units, sum(revenue) as revenue
               from '${LOCAL_NAME}' ${where} group by 1 order by 3 desc limit 10`, params),
        query(`select count(*) as rows, sum(units) as units, sum(revenue) as revenue from '${LOCAL_NAME}' ${where}`, params),
      ])
      setResults({
        monthly: monthly.rows,
        stores: stores.rows,
        products: products.rows,
        totals: totals.rows,
        ms: performance.now() - started,
      })
    }
    run().catch((e) => setError(String(e.message ?? e)))
  }, [loaded, region])

  const total = results?.totals[0]
  return (
    <main className="mx-auto max-w-5xl px-4 py-8">
      <h1 className="text-2xl font-semibold tracking-tight">Sales report</h1>
      <p className="mt-1 max-w-2xl text-sm text-[var(--muted)]">
        Two years of daily sales by store and product, written as one Parquet file by a nightly job. The page
        downloads the file through the app's handler and queries it here, in your browser, with DuckDB.
      </p>

      {error && <p className="mt-6 rounded-lg border border-red-500/40 p-3 text-sm">{error}</p>}
      {busy && <p className="mt-6 text-sm text-[var(--muted)]">{busy}</p>}
      {!busy && !error && !file && (
        <p className="mt-6 text-sm text-[var(--muted)]">
          No report yet. Run the <code>build-report</code> job from the admin page or with the <code>app_jobs</code> tool.
        </p>
      )}

      {file && loaded && (
        <div className="mt-6 grid gap-3 sm:grid-cols-4">
          <Stat label="File" value={bytes(loaded.bytes)} note={`${file.rows.toLocaleString()} rows, built ${when(file.created_at)}`} />
          <Stat label="Download" value={ms(loaded.fetchMs)} note="the whole file, through the handler" />
          <Stat label="Into DuckDB" value={ms(loaded.registerMs)} note="registerFileBuffer" />
          <Stat label="Four queries" value={results ? ms(results.ms) : '…'} note="run in this tab" />
        </div>
      )}

      {results && (
        <>
          <div className="mt-6 flex flex-wrap items-center gap-3">
            <label className="text-sm text-[var(--muted)]" htmlFor="region">Region</label>
            <select id="region" value={region} onChange={(e) => setRegion(e.target.value)}
              className="rounded-md border border-neutral-500/30 bg-transparent px-2 py-1 text-sm">
              <option value="">All regions</option>
              {regions.map((r) => <option key={r} value={r}>{r}</option>)}
            </select>
            {total && (
              <span className="text-sm text-[var(--muted)]">
                {Number(total.rows).toLocaleString()} rows, {Number(total.units).toLocaleString()} units,{' '}
                <span className="font-medium text-[var(--text)]">{money(Number(total.revenue))}</span>
              </span>
            )}
          </div>

          <div className="mt-4 grid items-start gap-4 lg:grid-cols-5">
            <Card title="Revenue by month" className="lg:col-span-3">
              <LineChart points={results.monthly.map((r) => ({ label: String(r.month), value: Number(r.revenue) }))} />
            </Card>
            <Card title="Revenue by store" className="lg:col-span-2">
              <BarChart bars={results.stores.map((r) => ({ label: String(r.store).replace('Store ', '#'), value: Number(r.revenue) }))} />
            </Card>
          </div>

          <Card title="Top ten products" className="mt-4">
            <table className="w-full text-left text-sm">
              <thead>
                <tr className="border-b border-neutral-500/20 text-xs uppercase tracking-wide text-[var(--muted)]">
                  <th className="py-1.5 font-medium">Product</th>
                  <th className="py-1.5 text-right font-medium">Units</th>
                  <th className="py-1.5 text-right font-medium">Revenue</th>
                </tr>
              </thead>
              <tbody>
                {results.products.map((r) => (
                  <tr key={String(r.product)} className="border-b border-neutral-500/10">
                    <td className="py-1.5">{String(r.product)}</td>
                    <td className="py-1.5 text-right tabular-nums">{Number(r.units).toLocaleString()}</td>
                    <td className="py-1.5 text-right tabular-nums">{money(Number(r.revenue))}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </Card>
        </>
      )}
    </main>
  )
}

function Stat({ label, value, note }: { label: string; value: string; note: string }) {
  return (
    <div className="rounded-xl border border-neutral-500/20 p-3">
      <div className="text-xs uppercase tracking-wide text-[var(--muted)]">{label}</div>
      <div className="mt-1 text-xl font-semibold tabular-nums">{value}</div>
      <div className="mt-0.5 text-xs text-[var(--muted)]">{note}</div>
    </div>
  )
}

function Card({ title, className = '', children }: { title: string; className?: string; children: ReactNode }) {
  return (
    <section className={`rounded-xl border border-neutral-500/20 p-4 ${className}`}>
      <h2 className="mb-3 text-sm font-semibold uppercase tracking-wide text-[var(--muted)]">{title}</h2>
      {children}
    </section>
  )
}
