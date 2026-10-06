import { useEffect, useRef, useState, type FormEvent } from 'react'
import { api, connectorUrl, when } from './api'
import { Button, CopyField, ErrorText, input, useLoad } from './ui'

type Line = { id: number; sku: string; name: string; quantity: number; unit_price_cents: number; line_cents: number }
type Order = {
  id: number
  customer: string
  status: 'draft' | 'submitted' | 'approved' | 'rejected'
  created_at: number
  submitted_at: number | null
  decided_at: number | null
  decided_by?: string | null
  lines?: Line[] | number
  total_cents: number
}
type Product = { sku: string; name: string; unit_price_cents: number }

const money = (cents: number) => (cents / 100).toLocaleString(undefined, { style: 'currency', currency: 'USD' })

const BADGE: Record<Order['status'], string> = {
  draft: 'bg-neutral-500/15 text-neutral-600 dark:text-neutral-300',
  submitted: 'bg-amber-500/15 text-amber-700 dark:text-amber-300',
  approved: 'bg-emerald-500/15 text-emerald-700 dark:text-emerald-300',
  rejected: 'bg-red-500/15 text-red-700 dark:text-red-300',
}

function Badge({ status }: { status: Order['status'] }) {
  return <span className={`rounded-full px-2 py-0.5 text-xs font-medium ${BADGE[status]}`}>{status}</span>
}

export default function App() {
  const me = useLoad(() => api('api/me'))
  const orders = useLoad<{ orders: Order[] }>(() => api('api/orders'))
  const products = useLoad<Product[]>(() => api('api/products'))
  const [selected, setSelected] = useState<number | null>(null)
  const [view, setView] = useState<'mine' | 'queue' | 'connect'>('mine')
  const [customer, setCustomer] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [menu, setMenu] = useState(false)
  const menuRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (selected === null && orders.data?.orders.length) setSelected(orders.data.orders[0].id)
  }, [orders.data, selected])
  useEffect(() => {
    const close = (e: MouseEvent) => menuRef.current && !menuRef.current.contains(e.target as Node) && setMenu(false)
    document.addEventListener('mousedown', close)
    return () => document.removeEventListener('mousedown', close)
  }, [])

  const create = async (e: FormEvent) => {
    e.preventDefault()
    setError(null)
    try {
      const { order } = await api('api/orders', { body: { customer } })
      setCustomer('')
      setSelected(order.id)
      orders.reload()
    } catch (err: any) {
      setError(err.message)
    }
  }

  return (
    <div className="min-h-screen bg-neutral-50 text-neutral-900 dark:bg-neutral-950 dark:text-neutral-100">
      <header className="flex items-center justify-between border-b border-neutral-500/15 px-6 py-3">
        <div className="flex items-center gap-6">
          <div className="flex items-center gap-2 font-semibold">
            <span className="text-xl">📦</span> Orders
          </div>
          <nav className="flex gap-1 text-sm">
            <Tab on={view === 'mine'} click={() => setView('mine')}>
              My orders
            </Tab>
            {me.data?.is_approver && (
              <Tab on={view === 'queue'} click={() => setView('queue')}>
                Approval queue
              </Tab>
            )}
          </nav>
        </div>
        <div className="flex items-center gap-3 text-sm text-neutral-500">
          {me.data?.user?.email}
          <div className="relative" ref={menuRef}>
            <button onClick={() => setMenu(!menu)} className="rounded-md px-2 py-1 text-lg leading-none hover:bg-neutral-500/10" aria-label="Menu">
              ⋯
            </button>
            {menu && (
              <div className="absolute right-0 z-10 mt-1 w-60 rounded-lg border border-neutral-500/20 bg-white p-1 text-neutral-900 shadow-lg dark:bg-neutral-900 dark:text-neutral-100">
                <button
                  onClick={() => {
                    setView('connect')
                    setMenu(false)
                  }}
                  className="block w-full rounded-md px-3 py-2 text-left hover:bg-neutral-500/10"
                >
                  Connect an AI assistant
                </button>
              </div>
            )}
          </div>
        </div>
      </header>

      {view === 'connect' && <Connect />}
      {view === 'queue' && <Queue onDecided={orders.reload} />}
      {view === 'mine' && (
        <div className="mx-auto grid max-w-6xl gap-6 px-6 py-8 md:grid-cols-[22rem_1fr]">
          <div className="space-y-3">
            <form onSubmit={create} className="flex gap-2">
              <input className={`${input} flex-1`} placeholder="Customer for a new order" value={customer} onChange={(e) => setCustomer(e.target.value)} />
              <Button type="submit">New</Button>
            </form>
            <ErrorText error={error ?? orders.error} />
            <ul className="space-y-1.5">
              {orders.data?.orders.map((o) => (
                <li key={o.id}>
                  <button
                    onClick={() => setSelected(o.id)}
                    className={`w-full rounded-lg border px-3 py-2 text-left ${
                      selected === o.id ? 'border-indigo-500 bg-indigo-500/5' : 'border-neutral-500/20 hover:bg-neutral-500/5'
                    }`}
                  >
                    <div className="flex items-center justify-between">
                      <span className="font-medium">{o.customer}</span>
                      <Badge status={o.status} />
                    </div>
                    <div className="mt-0.5 flex justify-between text-xs text-neutral-500">
                      <span>
                        #{o.id} · {o.lines as number} lines
                      </span>
                      <span className="font-mono">{money(o.total_cents)}</span>
                    </div>
                  </button>
                </li>
              ))}
            </ul>
            {orders.data?.orders.length === 0 && <p className="text-sm text-neutral-500">No orders yet. Start one above.</p>}
          </div>
          {selected !== null && <Detail id={selected} products={products.data ?? []} onChange={orders.reload} />}
        </div>
      )}
    </div>
  )
}

function Tab({ on, click, children }: { on: boolean; click: () => void; children: React.ReactNode }) {
  return (
    <button onClick={click} className={`rounded-md px-3 py-1.5 ${on ? 'bg-neutral-500/15 font-medium' : 'text-neutral-500 hover:bg-neutral-500/10'}`}>
      {children}
    </button>
  )
}

function Detail({ id, products, onChange }: { id: number; products: Product[]; onChange: () => void }) {
  const { data, error, reload } = useLoad<{ order: Order }>(() => api(`api/orders/${id}`), [id])
  const [sku, setSku] = useState('PAL-STD')
  const [quantity, setQuantity] = useState(1)
  const [problem, setProblem] = useState<string | null>(null)
  const order = data?.order

  const act = async (path: string, init?: { method?: string; body?: unknown }) => {
    setProblem(null)
    try {
      await api(path, init)
      reload()
      onChange()
    } catch (err: any) {
      setProblem(err.message)
    }
  }

  if (error) return <ErrorText error={error} />
  if (!order) return null
  const lines = (order.lines as Line[]) ?? []
  const draft = order.status === 'draft'

  return (
    <section className="rounded-xl border border-neutral-500/20 bg-white/60 p-5 shadow-sm dark:bg-neutral-900/60">
      <div className="flex items-start justify-between">
        <div>
          <h2 className="text-xl font-semibold">{order.customer}</h2>
          <p className="text-sm text-neutral-500">
            Order #{order.id} · started {when(order.created_at)}
            {order.submitted_at && ` · submitted ${when(order.submitted_at)}`}
            {order.decided_by && ` · ${order.status} by ${order.decided_by}`}
          </p>
        </div>
        <Badge status={order.status} />
      </div>

      <table className="mt-5 w-full text-sm">
        <thead>
          <tr className="border-b border-neutral-500/20 text-left text-xs uppercase tracking-wide text-neutral-500">
            <th className="py-1.5">Product</th>
            <th className="py-1.5 text-right">Qty</th>
            <th className="py-1.5 text-right">Price</th>
            <th className="py-1.5 text-right">Line</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {lines.map((l) => (
            <tr key={l.id} className="border-b border-neutral-500/10">
              <td className="py-1.5">
                {l.name} <span className="font-mono text-xs text-neutral-500">{l.sku}</span>
              </td>
              <td className="py-1.5 text-right">{l.quantity}</td>
              <td className="py-1.5 text-right font-mono">{money(l.unit_price_cents)}</td>
              <td className="py-1.5 text-right font-mono">{money(l.line_cents)}</td>
              <td className="w-8 text-right">
                {draft && (
                  <button className="text-neutral-400 hover:text-red-500" onClick={() => act(`api/orders/${id}/lines/${l.id}`, { method: 'DELETE' })} aria-label="Remove line">
                    ✕
                  </button>
                )}
              </td>
            </tr>
          ))}
          {lines.length === 0 && (
            <tr>
              <td colSpan={5} className="py-3 text-neutral-500">
                No lines yet.
              </td>
            </tr>
          )}
        </tbody>
        <tfoot>
          <tr>
            <td colSpan={3} className="pt-3 text-right font-medium">
              Total
            </td>
            <td className="pt-3 text-right font-mono font-semibold">{money(order.total_cents)}</td>
            <td />
          </tr>
        </tfoot>
      </table>

      {draft && (
        <div className="mt-5 flex flex-wrap items-center gap-2">
          <select className={input} value={sku} onChange={(e) => setSku(e.target.value)}>
            {products.map((p) => (
              <option key={p.sku} value={p.sku}>
                {p.name} ({money(p.unit_price_cents)})
              </option>
            ))}
          </select>
          <input className={`${input} w-20`} type="number" min={1} value={quantity} onChange={(e) => setQuantity(Number(e.target.value))} />
          <Button kind="quiet" onClick={() => act(`api/orders/${id}/lines`, { body: { sku, quantity } })}>
            Add line
          </Button>
          <div className="flex-1" />
          <Button onClick={() => act(`api/orders/${id}/submit`, { body: {} })}>Submit for approval</Button>
        </div>
      )}
      <div className="mt-3">
        <ErrorText error={problem} />
      </div>
    </section>
  )
}

function Queue({ onDecided }: { onDecided: () => void }) {
  const { data, error, reload } = useLoad<{ orders: Order[] }>(() => api('api/queue'))
  const decide = async (id: number, decision: string) => {
    await api(`api/orders/${id}/decide`, { body: { decision } })
    reload()
    onDecided()
  }
  return (
    <div className="mx-auto max-w-4xl px-6 py-8">
      <h1 className="text-xl font-semibold">Waiting for a decision</h1>
      <p className="mt-1 text-sm text-neutral-500">Everyone's submitted orders. Only the approver role sees this.</p>
      <ErrorText error={error} />
      <ul className="mt-5 space-y-2">
        {data?.orders.map((o) => (
          <li key={o.id} className="flex items-center justify-between rounded-lg border border-neutral-500/20 px-4 py-3">
            <div>
              <div className="font-medium">{o.customer}</div>
              <div className="text-xs text-neutral-500">
                #{o.id} · {o.lines as number} lines · submitted {when(o.submitted_at)}
              </div>
            </div>
            <div className="flex items-center gap-3">
              <span className="font-mono">{money(o.total_cents)}</span>
              <Button kind="quiet" onClick={() => decide(o.id, 'reject')}>
                Reject
              </Button>
              <Button onClick={() => decide(o.id, 'approve')}>Approve</Button>
            </div>
          </li>
        ))}
      </ul>
      {data?.orders.length === 0 && <p className="mt-5 text-sm text-neutral-500">Nothing is waiting.</p>}
    </div>
  )
}

function Connect() {
  return (
    <div className="mx-auto max-w-2xl px-6 py-8">
      <h1 className="text-xl font-semibold">Connect an AI assistant</h1>
      <p className="mt-1 text-sm text-neutral-500">
        Add this link as a connector in Claude or ChatGPT and sign in with your account. The assistant can then create orders,
        add lines, submit them and list yours, as you. It cannot see anyone else's orders.
      </p>
      <div className="mt-5">
        <CopyField value={connectorUrl()} />
      </div>
      <ul className="mt-5 space-y-1 text-sm">
        <li>
          <b>create_order</b>: start a draft, with optional lines
        </li>
        <li>
          <b>add_line</b>: add a product to a draft
        </li>
        <li>
          <b>submit_order</b>: send a draft to an approver
        </li>
        <li>
          <b>my_orders</b>: list your orders with totals
        </li>
      </ul>
    </div>
  )
}
