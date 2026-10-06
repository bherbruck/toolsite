import { useState, type FormEvent } from 'react'
import { api, bytes, connectorUrl, when } from './api'
import { Button, Card, Code, CopyField, ErrorText, Screen, Table, input, useLoad } from './ui'

export type Me = {
  user: { id: string; email: string } | null
  role: string | null
  roles: string[]
  is_manager: boolean
  sql: { current_user: unknown; current_email: unknown; current_role: unknown }
  locations: { code: string; name: string }[]
}

// --- SQL and migrations ----------------------------------------------------

export function Database() {
  const { data, error } = useLoad(() => api('api/overview'))
  return (
    <Screen
      title="Database"
      lead={
        <>
          Every app has its own SQLite database. The schema comes from numbered files in{' '}
          <Code>migrations/</Code>, applied once each at deploy. The handler's own <Code>db.query</Code> sees every
          row, so this screen shows totals only.
        </>
      }
    >
      <ErrorText error={error} />
      <Card title="Orders by location and status">
        <Table rows={data?.totals ?? []} />
      </Card>
      <Card title="Tables and views">
        <Table rows={data?.tables ?? []} />
        <p className="mt-3 text-xs text-neutral-500">
          <Code>my_orders</Code> is generated from the policy in <Code>toolsite.toml</Code>.{' '}
          <Code>my_locations</Code> is hand-written in <Code>001_initial.sql</Code> and declared as a view.
        </p>
      </Card>
    </Screen>
  )
}

// --- row-level access ------------------------------------------------------

export function Orders({ me }: { me: Me | null }) {
  const orders = useLoad(() => api('api/orders'))
  const [form, setForm] = useState({ customer: '', item: '', quantity: 1, location: '' })
  const [error, setError] = useState<string | null>(null)

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setError(null)
    try {
      await api('api/orders', { body: { ...form, location: form.location || undefined } })
      setForm({ ...form, customer: '', item: '' })
      orders.reload()
    } catch (err: any) {
      setError(err.message)
    }
  }
  const setStatus = async (id: number, status: string) => {
    try {
      await api(`api/orders/${id}/status`, { body: { status } })
      orders.reload()
    } catch (err: any) {
      setError(err.message)
    }
  }

  return (
    <Screen
      title="Orders"
      lead={
        <>
          A row-level policy in <Code>toolsite.toml</Code> gives each person the orders of the locations the{' '}
          <Code>members</Code> table puts them in. The handler reads and writes through <Code>my_orders</Code> with{' '}
          <Code>db.query-scoped</Code>, so two people see different rows from the same SQL.
        </>
      }
    >
      <Card title="Your locations">
        {me?.locations.length ? (
          <div className="flex flex-wrap gap-2">
            {me.locations.map((l) => (
              <span key={l.code} className="rounded-full bg-indigo-500/10 px-3 py-1 text-sm text-indigo-600 dark:text-indigo-300">
                {l.name} <span className="font-mono text-xs opacity-70">{l.code}</span>
              </span>
            ))}
          </div>
        ) : (
          <p className="text-sm text-neutral-500">
            You are not a member of a location yet, so you see no orders. A manager places people on the Admin screen.
          </p>
        )}
      </Card>
      <Card title="New order">
        <form onSubmit={submit} className="flex flex-wrap items-end gap-2">
          <input className={input} placeholder="Customer" value={form.customer} onChange={(e) => setForm({ ...form, customer: e.target.value })} />
          <input className={input} placeholder="Item" value={form.item} onChange={(e) => setForm({ ...form, item: e.target.value })} />
          <input
            className={`${input} w-20`}
            type="number"
            min={1}
            value={form.quantity}
            onChange={(e) => setForm({ ...form, quantity: Number(e.target.value) })}
          />
          <select className={input} value={form.location} onChange={(e) => setForm({ ...form, location: e.target.value })}>
            <option value="">First location</option>
            {me?.locations.map((l) => (
              <option key={l.code} value={l.code}>
                {l.name}
              </option>
            ))}
            <option value="south">south (try one you are not in)</option>
          </select>
          <Button type="submit">Create</Button>
        </form>
        <div className="mt-3">
          <ErrorText error={error} />
        </div>
      </Card>
      <Card title="Orders you can see">
        <ErrorText error={orders.error} />
        <div className="overflow-x-auto">
          <table className="w-full text-left text-sm">
            <thead>
              <tr className="border-b border-neutral-500/20 text-xs uppercase tracking-wide text-neutral-500">
                <th className="px-2 py-1.5">#</th>
                <th className="px-2 py-1.5">Location</th>
                <th className="px-2 py-1.5">Customer</th>
                <th className="px-2 py-1.5">Item</th>
                <th className="px-2 py-1.5">Qty</th>
                <th className="px-2 py-1.5">Status</th>
              </tr>
            </thead>
            <tbody>
              {(orders.data?.orders ?? []).map((o: any) => (
                <tr key={o.id} className="border-b border-neutral-500/10">
                  <td className="px-2 py-1.5 font-mono text-xs">{o.id}</td>
                  <td className="px-2 py-1.5">{o.location}</td>
                  <td className="px-2 py-1.5">{o.customer}</td>
                  <td className="px-2 py-1.5">{o.item}</td>
                  <td className="px-2 py-1.5">{o.quantity}</td>
                  <td className="px-2 py-1.5">
                    <select className={`${input} py-0.5`} value={o.status} onChange={(e) => setStatus(o.id, e.target.value)}>
                      <option value="open">open</option>
                      <option value="shipped">shipped</option>
                      <option value="cancelled">cancelled</option>
                    </select>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {orders.data?.orders?.length === 0 && <p className="py-3 text-sm text-neutral-500">No orders you can see.</p>}
        </div>
      </Card>
    </Screen>
  )
}

export function SqlConsole() {
  const [sql, setSql] = useState('select location, customer, item, quantity, status from my_orders order by id desc')
  const [result, setResult] = useState<any>(null)
  const [error, setError] = useState<string | null>(null)
  const run = async (e: FormEvent) => {
    e.preventDefault()
    setError(null)
    try {
      setResult(await api('api/sql', { body: { sql } }))
    } catch (err: any) {
      setResult(null)
      setError(err.message)
    }
  }
  const rows = result ? result.rows.map((r: unknown[]) => Object.fromEntries(result.columns.map((c: string, i: number) => [c, r[i]]))) : []
  return (
    <Screen
      title="SQL as you"
      lead={
        <>
          Type any statement. It runs with <Code>db.query-scoped</Code>: inside the declared views and nothing else.
          Try <Code>select * from orders</Code> to see the base table refused, or <Code>select * from my_locations</Code>.
        </>
      }
    >
      <Card>
        <form onSubmit={run} className="space-y-2">
          <textarea className={`${input} h-24 w-full font-mono`} value={sql} onChange={(e) => setSql(e.target.value)} />
          <Button type="submit">Run</Button>
        </form>
      </Card>
      <ErrorText error={error} />
      {result && (
        <Card title={`${rows.length} rows${result.truncated ? ' (truncated)' : ''}`}>
          <Table rows={rows} columns={result.columns} />
        </Card>
      )}
    </Screen>
  )
}

// --- files -------------------------------------------------------------------

export function Files() {
  const files = useLoad(() => api('api/files'))
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const upload = async (file: File) => {
    setError(null)
    setBusy(true)
    try {
      const name = file.name.replace(/[^A-Za-z0-9._-]/g, '-').replace(/^\.+/, '')
      const ticket = await api('api/files', { body: { name } })
      // The bytes go straight to storage. The handler only decided who may.
      const res = await fetch(ticket.url, {
        method: 'PUT',
        headers: { 'content-type': file.type || 'application/octet-stream' },
        body: file,
      })
      if (!res.ok) throw new Error(`upload failed: HTTP ${res.status} ${await res.text()}`)
      files.reload()
    } catch (err: any) {
      setError(err.message)
    } finally {
      setBusy(false)
    }
  }
  const remove = async (name: string) => {
    await api(`api/files/${encodeURIComponent(name)}`, { method: 'DELETE' }).catch((e) => setError(e.message))
    files.reload()
  }

  return (
    <Screen
      title="Files"
      lead={
        <>
          The handler asks for <Code>blobs.upload-url</Code> and the browser PUTs the file there, so the bytes never pass
          through the handler. To serve one, the handler answers with the header <Code>x-toolsite-blob</Code> and the
          platform streams it.
        </>
      }
    >
      <Card title="Upload">
        <input type="file" disabled={busy} onChange={(e) => e.target.files?.[0] && upload(e.target.files[0])} className="text-sm" />
        <div className="mt-3">
          <ErrorText error={error ?? files.error} />
        </div>
      </Card>
      <Card title="Stored">
        {(files.data?.files ?? []).length === 0 && <p className="text-sm text-neutral-500">Nothing stored yet.</p>}
        <ul className="divide-y divide-neutral-500/10">
          {(files.data?.files ?? []).map((f: any) => (
            <li key={f.key} className="flex items-center justify-between gap-2 py-2 text-sm">
              <a className="font-mono text-indigo-600 hover:underline dark:text-indigo-300" href={`api/files/${encodeURIComponent(f.name)}`} target="_blank" rel="noreferrer">
                {f.name}
              </a>
              <span className="text-xs text-neutral-500">
                {bytes(f.size)} {f.uploaded_by ? `by ${f.uploaded_by}` : ''}
              </span>
              <Button kind="quiet" onClick={() => remove(f.name)}>
                Delete
              </Button>
            </li>
          ))}
        </ul>
      </Card>
    </Screen>
  )
}

// --- settings and fetch ----------------------------------------------------

export function Settings() {
  const { data, error } = useLoad(() => api('api/settings'))
  return (
    <Screen
      title="Settings"
      lead={
        <>
          Settings are values the site owner enters for this app, sealed at rest. The handler reads them with{' '}
          <Code>secrets.get</Code>. This screen shows only whether a value is set: the value never goes to a browser.
        </>
      }
    >
      <ErrorText error={error} />
      <Card title="GREETING">
        <p className="text-sm">
          {data ? (
            data.greeting_set ? (
              <span className="text-emerald-600 dark:text-emerald-400">Set.</span>
            ) : (
              <span className="text-amber-600 dark:text-amber-400">Not set.</span>
            )
          ) : (
            'Loading.'
          )}{' '}
          Set it from the app's Settings tab in the site admin.
        </p>
      </Card>
      <Card title="Names that exist">
        {data?.names?.length ? (
          <div className="flex flex-wrap gap-2">
            {data.names.map((n: string) => (
              <Code key={n}>{n}</Code>
            ))}
          </div>
        ) : (
          <p className="text-sm text-neutral-500">No settings yet.</p>
        )}
      </Card>
    </Screen>
  )
}

export function Fetch() {
  const [url, setUrl] = useState('https://api.github.com/zen')
  const [result, setResult] = useState<any>(null)
  const go = async (e: FormEvent) => {
    e.preventDefault()
    setResult(await api('api/fetch', { body: { url } }).catch((err) => ({ ok: false, error: err.message })))
  }
  return (
    <Screen
      title="Outbound fetch"
      lead={
        <>
          A handler reaches other services with <Code>fetch.send</Code>, but only the hosts under{' '}
          <Code>allow_http</Code> in <Code>toolsite.toml</Code>. This app allows <Code>api.github.com</Code>. Try another
          host to see the refusal.
        </>
      }
    >
      <Card>
        <form onSubmit={go} className="flex gap-2">
          <input className={`${input} flex-1 font-mono`} value={url} onChange={(e) => setUrl(e.target.value)} />
          <Button type="submit">Fetch</Button>
          <Button kind="quiet" onClick={() => setUrl('https://example.com/')}>
            Try a refused host
          </Button>
        </form>
      </Card>
      {result && (
        <Card title={result.ok ? `HTTP ${result.status}` : 'Not fetched'}>
          <pre className="whitespace-pre-wrap font-mono text-sm">{result.ok ? result.body : result.error}</pre>
        </Card>
      )}
    </Screen>
  )
}

// --- identity, jobs, routes, tools ------------------------------------------

export function Identity({ me }: { me: Me | null }) {
  return (
    <Screen
      title="Identity and roles"
      lead={
        <>
          The handler asks <Code>identity.current-user</Code> and <Code>identity.current-role</Code>. The same answers
          are bound in SQL as <Code>current_user()</Code>, <Code>current_email()</Code> and <Code>current_role()</Code>.
          A visitor cannot forge them.
        </>
      }
    >
      <Card title="You">
        <dl className="grid grid-cols-[10rem_1fr] gap-y-1 text-sm">
          <dt className="text-neutral-500">Email</dt>
          <dd>{me?.user?.email ?? 'not signed in'}</dd>
          <dt className="text-neutral-500">Account id</dt>
          <dd className="font-mono text-xs">{me?.user?.id ?? 'none'}</dd>
          <dt className="text-neutral-500">Role on this app</dt>
          <dd>{me?.role ?? 'no grant'}</dd>
        </dl>
      </Card>
      <Card title="Declared roles">
        <p className="mb-2 text-sm text-neutral-500">
          From <Code>roles</Code> in <Code>toolsite.toml</Code>. The site admin sees these as suggestions when granting
          access. A role is a string the app interprets; the platform does not.
        </p>
        <div className="flex gap-2">
          {me?.roles.map((r) => (
            <span key={r} className={`rounded-full px-3 py-1 text-sm ${r === me.role ? 'bg-indigo-600 text-white' : 'bg-neutral-500/10'}`}>
              {r}
            </span>
          ))}
        </div>
      </Card>
      <Card title="Asked of SQL">
        <Table rows={me ? [me.sql as Record<string, unknown>] : []} />
      </Card>
    </Screen>
  )
}

export function Jobs() {
  const { data, error, reload } = useLoad(() => api('api/heartbeats'))
  return (
    <Screen
      title="Scheduled job"
      lead={
        <>
          <Code>[[job]]</Code> in <Code>toolsite.toml</Code> calls <Code>/api/heartbeat</Code> every five minutes,
          through the same handler a request uses. The host marks the call with <Code>x-toolsite-scheduled</Code>, which
          a visitor cannot send.
        </>
      }
    >
      <ErrorText error={error} />
      <Card title="Last beats" actions={<Button kind="quiet" onClick={reload}>Reload</Button>}>
        {(data?.beats ?? []).length === 0 ? (
          <p className="text-sm text-neutral-500">No beat yet. Run the job now from the app's Jobs tab in the site admin.</p>
        ) : (
          <ul className="space-y-1 text-sm">
            {data.beats.map((b: any) => (
              <li key={b.id}>
                <span className="font-mono text-xs text-neutral-500">#{b.id}</span> {when(b.at)}
              </li>
            ))}
          </ul>
        )}
      </Card>
    </Screen>
  )
}

export function Routes() {
  const status = useLoad(() => api('status'))
  return (
    <Screen
      title="Route rules"
      lead={
        <>
          The app needs a signed-in account. <Code>[[route]]</Code> entries change that per path: <Code>/status</Code> is
          public, for a monitor with no account. <Code>/admin</Code> and <Code>/api/admin</Code> need a grant on this app,
          and the handler then asks for the manager role.
        </>
      }
    >
      <Card title="/status (public)">
        <pre className="font-mono text-sm">{status.data ? JSON.stringify(status.data, null, 2) : status.error ?? 'Loading.'}</pre>
        <p className="mt-2 text-sm">
          <a className="text-indigo-600 hover:underline dark:text-indigo-300" href="status" target="_blank" rel="noreferrer">
            Open in a private window
          </a>{' '}
          to see it answer with no account.
        </p>
      </Card>
      <Card title="/admin (restricted)">
        <a className="text-sm text-indigo-600 hover:underline dark:text-indigo-300" href="admin">
          Open the server-rendered admin page
        </a>
      </Card>
    </Screen>
  )
}

export function Admin({ me }: { me: Me | null }) {
  const members = useLoad(() => api('api/admin/members'))
  const [email, setEmail] = useState('')
  const [location, setLocation] = useState('north')
  const [error, setError] = useState<string | null>(null)
  const place = async (e: FormEvent) => {
    e.preventDefault()
    setError(null)
    try {
      await api('api/admin/members', { body: { email, location } })
      setEmail('')
      members.reload()
    } catch (err: any) {
      setError(err.message)
    }
  }
  return (
    <Screen
      title="Admin"
      lead={
        <>
          Who works where. This calls <Code>/api/admin/members</Code>, which a route rule restricts to people with a
          grant, and which the handler restricts to the manager role.
        </>
      }
    >
      {!me?.is_manager && <ErrorText error="You do not have the manager role, so the calls below are refused." />}
      <Card title="Place a person">
        <form onSubmit={place} className="flex flex-wrap gap-2">
          <input className={`${input} flex-1`} placeholder="person@example.com" value={email} onChange={(e) => setEmail(e.target.value)} />
          <select className={input} value={location} onChange={(e) => setLocation(e.target.value)}>
            <option value="north">North yard</option>
            <option value="south">South yard</option>
            <option value="">Remove</option>
          </select>
          <Button type="submit">Save</Button>
        </form>
        <div className="mt-3">
          <ErrorText error={error} />
        </div>
      </Card>
      <Card title="Members">
        <ErrorText error={members.error} />
        <Table rows={members.data?.members ?? []} />
      </Card>
    </Screen>
  )
}

export function Connect() {
  return (
    <Screen
      title="Connect an AI assistant"
      lead={
        <>
          This app offers MCP tools, declared as <Code>[[tool]]</Code> in <Code>toolsite.toml</Code>. Add the link below as
          a connector in Claude or ChatGPT and sign in with your account. Each tool runs as you, under the same access and
          row-level policies as this page.
        </>
      }
    >
      <Card title="Connector link">
        <CopyField value={connectorUrl()} />
      </Card>
      <Card title="Tools">
        <ul className="space-y-2 text-sm">
          <li>
            <Code>create_order</Code> creates an order at one of your locations. It writes.
          </li>
          <li>
            <Code>list_my_orders</Code> lists the orders you can see. It only reads.
          </li>
        </ul>
      </Card>
    </Screen>
  )
}
