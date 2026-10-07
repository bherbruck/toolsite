import { useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react'
import mqtt, { type MqttClient } from 'mqtt'

// Two sources, kept apart:
//
// - The page is an MQTT client itself. MQTT.js connects over the WebSocket
//   the app declares at `mqtt`, with the subprotocol "mqtt". The person is
//   already signed in to toolsite, so the broker takes them without a
//   device token. What this page subscribes to arrives here live.
// - `api/status` is what the broker last saved: who is connected and the
//   most recent publishes from every client. It is polled, since a request
//   runs in a fresh instance and only sees what the broker wrote down.

type Client = {
  client_id: string
  label: string | null
  username: string | null
  transport: 'tcp' | 'websocket'
  remote: string | null
  connected_ms: number
}

type Seen = {
  at_ms: number
  client_id: string
  topic: string
  qos: number
  retain: boolean
  size: number
  preview: string
}

type Status = {
  running: boolean
  started_ms?: number
  updated_ms?: number
  publishes?: number
  clients: Client[]
  recent: Seen[]
}

type Received = { id: number; at: number; topic: string; payload: string; qos: number; retain: boolean }
type Sub = { filter: string; qos: 0 | 1 | 2 }
type Link = 'connecting' | 'online' | 'offline'

/** The app's socket, from the page's own address, so it is right under
 * /p/<slug>/ on any site. */
function socketUrl(): string {
  const url = new URL('mqtt', document.baseURI)
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:'
  return url.href
}

function ago(ms: number | undefined): string {
  if (!ms) return 'never'
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000))
  if (s < 60) return `${s}s ago`
  if (s < 3600) return `${Math.round(s / 60)}m ago`
  return `${Math.round(s / 3600)}h ago`
}

export default function App() {
  const [status, setStatus] = useState<Status | null>(null)
  const [link, setLink] = useState<Link>('connecting')
  const [linkError, setLinkError] = useState('')
  const [subs, setSubs] = useState<Sub[]>([])
  const [received, setReceived] = useState<Received[]>([])
  const client = useRef<MqttClient | null>(null)

  useEffect(() => {
    let stopped = false
    const poll = async () => {
      try {
        const res = await fetch('api/status')
        if (res.ok && !stopped) setStatus(await res.json())
      } catch {
        // The next poll tries again.
      }
    }
    poll()
    const timer = setInterval(poll, 3000)
    return () => {
      stopped = true
      clearInterval(timer)
    }
  }, [])

  useEffect(() => {
    const c = mqtt.connect(socketUrl(), {
      protocolVersion: 4,
      clientId: `web-${Math.random().toString(16).slice(2, 10)}`,
      keepalive: 30,
      reconnectPeriod: 3000,
    })
    client.current = c
    c.on('connect', () => {
      setLink('online')
      setLinkError('')
    })
    c.on('reconnect', () => setLink('connecting'))
    c.on('offline', () => setLink('offline'))
    c.on('close', () => setLink('offline'))
    c.on('error', (e) => setLinkError(e.message))
    c.on('message', (topic, payload, packet) => {
      const item = {
        id: Date.now() + Math.random(),
        at: Date.now(),
        topic,
        payload: new TextDecoder().decode(payload),
        qos: packet.qos,
        retain: packet.retain,
      }
      setReceived((r) => [item, ...r].slice(0, 100))
    })
    return () => {
      c.end(true)
      client.current = null
    }
  }, [])

  // A reconnect starts a clean session, so subscriptions are made again.
  useEffect(() => {
    if (link !== 'online') return
    for (const s of subs) client.current?.subscribe(s.filter, { qos: s.qos })
  }, [link]) // eslint-disable-line react-hooks/exhaustive-deps

  const subscribe = (sub: Sub) => {
    if (subs.some((s) => s.filter === sub.filter)) return
    client.current?.subscribe(sub.filter, { qos: sub.qos }, (err) => {
      if (err) setLinkError(err.message)
      else setSubs((s) => [...s, sub])
    })
  }

  const unsubscribe = (filter: string) => {
    client.current?.unsubscribe(filter)
    setSubs((s) => s.filter((x) => x.filter !== filter))
  }

  const publish = (topic: string, payload: string, qos: 0 | 1 | 2, retain: boolean) => {
    client.current?.publish(topic, payload, { qos, retain }, (err) => err && setLinkError(err.message))
  }

  return (
    <div className="min-h-screen bg-slate-50 text-slate-900 dark:bg-slate-950 dark:text-slate-100">
      <header className="border-b border-slate-200 bg-white px-4 py-3 dark:border-slate-800 dark:bg-slate-900">
        <div className="mx-auto flex max-w-6xl flex-wrap items-center gap-x-6 gap-y-2">
          <h1 className="text-lg font-semibold">MQTT broker</h1>
          <span className="text-sm text-slate-500">
            {status?.running ? (
              <>
                Running since {new Date(status.started_ms!).toLocaleString()} · {status.publishes ?? 0} publishes ·
                saved {ago(status.updated_ms)}
              </>
            ) : (
              'Not running. The first connection starts it.'
            )}
          </span>
          <span className="ml-auto flex items-center gap-2 text-sm">
            <Dot on={link === 'online'} />
            This page: {link}
            {linkError && <span className="text-red-600 dark:text-red-400">({linkError})</span>}
          </span>
        </div>
      </header>

      <main className="mx-auto grid max-w-6xl gap-4 p-4 lg:grid-cols-2">
        <Panel title="Publish">
          <PublishForm disabled={link !== 'online'} onPublish={publish} />
        </Panel>

        <Panel title="Subscribe">
          <SubscribeForm disabled={link !== 'online'} onSubscribe={subscribe} />
          {subs.length > 0 && (
            <ul className="mt-3 flex flex-wrap gap-2">
              {subs.map((s) => (
                <li key={s.filter} className="flex items-center gap-2 rounded bg-slate-100 px-2 py-1 font-mono text-sm dark:bg-slate-800">
                  {s.filter} <span className="text-slate-500">q{s.qos}</span>
                  <button className="text-slate-500 hover:text-red-600" onClick={() => unsubscribe(s.filter)} aria-label={`Unsubscribe from ${s.filter}`}>
                    ×
                  </button>
                </li>
              ))}
            </ul>
          )}
        </Panel>

        <Panel title={`Received here (${received.length})`}>
          {received.length === 0 ? (
            <Muted>Subscribe to a topic, for example <code>#</code>, to see messages arrive.</Muted>
          ) : (
            <Messages rows={received.map((r) => ({ key: r.id, at: r.at, who: '', topic: r.topic, qos: r.qos, retain: r.retain, text: r.payload }))} />
          )}
        </Panel>

        <Panel title={`Connected clients (${status?.clients.length ?? 0})`}>
          {!status?.clients.length ? (
            <Muted>Nobody is connected.</Muted>
          ) : (
            <table className="w-full text-left text-sm">
              <thead className="text-slate-500">
                <tr>
                  <th className="py-1 pr-2 font-normal">Client id</th>
                  <th className="py-1 pr-2 font-normal">Who</th>
                  <th className="py-1 pr-2 font-normal">Via</th>
                  <th className="py-1 font-normal">Since</th>
                </tr>
              </thead>
              <tbody>
                {status.clients.map((c) => (
                  <tr key={c.client_id} className="border-t border-slate-100 dark:border-slate-800">
                    <td className="py-1 pr-2 font-mono">{c.client_id}</td>
                    <td className="py-1 pr-2">{c.label ?? '-'}{c.username && c.username !== c.label ? ` (${c.username})` : ''}</td>
                    <td className="py-1 pr-2">{c.transport === 'tcp' ? `TCP ${c.remote ?? ''}` : 'WebSocket'}</td>
                    <td className="py-1 text-slate-500">{ago(c.connected_ms)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </Panel>

        <Panel title="Recent on the broker" wide>
          {!status?.recent.length ? (
            <Muted>No publishes yet.</Muted>
          ) : (
            <Messages rows={status.recent.map((r, i) => ({ key: i, at: r.at_ms, who: r.client_id, topic: r.topic, qos: r.qos, retain: r.retain, text: r.preview + (r.size > r.preview.length ? ' ...' : '') }))} />
          )}
        </Panel>

        <Panel title="Devices" wide>
          <div className="space-y-2 text-sm">
            <p>
              A device connects over TCP, on the port the site's owner mapped to this app, with a device token as its
              MQTT password. Mint one token per device on this app's <b>Connections</b> tab on <code>/admin</code>.
              The username is free: the broker shows the token's label.
            </p>
            <pre className="overflow-x-auto rounded bg-slate-100 p-2 dark:bg-slate-800">
              mosquitto_sub -h HOST -p PORT -u sensor-1 -P tsv_... -t 'sensors/#' -v{'\n'}
              mosquitto_pub -h HOST -p PORT -u sensor-1 -P tsv_... -t sensors/temp -m 21.5
            </pre>
            <p className="text-slate-500">
              Browsers use the WebSocket at <code className="break-all">{socketUrl()}</code> with the subprotocol
              <code> mqtt</code>, signed in as themselves.
            </p>
          </div>
        </Panel>
      </main>
    </div>
  )
}

function PublishForm({ disabled, onPublish }: { disabled: boolean; onPublish: (t: string, p: string, q: 0 | 1 | 2, r: boolean) => void }) {
  const [topic, setTopic] = useState('demo/hello')
  const [payload, setPayload] = useState('Hello from the browser')
  const [qos, setQos] = useState<0 | 1 | 2>(0)
  const [retain, setRetain] = useState(false)
  const submit = (e: FormEvent) => {
    e.preventDefault()
    if (topic.trim()) onPublish(topic.trim(), payload, qos, retain)
  }
  return (
    <form onSubmit={submit} className="space-y-2">
      <Input label="Topic" value={topic} onChange={setTopic} />
      <Input label="Payload" value={payload} onChange={setPayload} />
      <div className="flex flex-wrap items-center gap-4">
        <QosSelect value={qos} onChange={setQos} />
        <label className="flex items-center gap-2 text-sm">
          <input type="checkbox" checked={retain} onChange={(e) => setRetain(e.target.checked)} /> Retain
        </label>
        <Button disabled={disabled}>Publish</Button>
      </div>
    </form>
  )
}

function SubscribeForm({ disabled, onSubscribe }: { disabled: boolean; onSubscribe: (s: Sub) => void }) {
  const [filter, setFilter] = useState('#')
  const [qos, setQos] = useState<0 | 1 | 2>(0)
  const submit = (e: FormEvent) => {
    e.preventDefault()
    if (filter.trim()) onSubscribe({ filter: filter.trim(), qos })
  }
  return (
    <form onSubmit={submit} className="flex flex-wrap items-end gap-4">
      <div className="min-w-48 flex-1">
        <Input label="Topic filter" value={filter} onChange={setFilter} />
      </div>
      <QosSelect value={qos} onChange={setQos} />
      <Button disabled={disabled}>Subscribe</Button>
    </form>
  )
}

function Messages({ rows }: { rows: { key: number; at: number; who: string; topic: string; qos: number; retain: boolean; text: string }[] }) {
  return (
    <ul className="max-h-80 space-y-1 overflow-y-auto text-sm">
      {rows.map((r) => (
        <li key={r.key} className="rounded border border-slate-100 px-2 py-1 dark:border-slate-800">
          <div className="flex flex-wrap gap-x-3 text-xs text-slate-500">
            <span className="font-mono text-slate-700 dark:text-slate-300">{r.topic}</span>
            <span>q{r.qos}</span>
            {r.retain && <span>retained</span>}
            {r.who && <span>from {r.who}</span>}
            <span className="ml-auto">{new Date(r.at).toLocaleTimeString()}</span>
          </div>
          <div className="break-all font-mono">{r.text || <span className="text-slate-400">(empty)</span>}</div>
        </li>
      ))}
    </ul>
  )
}

function Panel({ title, wide, children }: { title: string; wide?: boolean; children: ReactNode }) {
  return (
    <section className={`rounded-lg border border-slate-200 bg-white p-4 dark:border-slate-800 dark:bg-slate-900 ${wide ? 'lg:col-span-2' : ''}`}>
      <h2 className="mb-3 font-medium">{title}</h2>
      {children}
    </section>
  )
}

function Input({ label, value, onChange }: { label: string; value: string; onChange: (v: string) => void }) {
  return (
    <label className="block text-sm">
      <span className="text-slate-500">{label}</span>
      <input
        className="mt-1 w-full rounded border border-slate-300 bg-transparent px-2 py-1 font-mono dark:border-slate-700"
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
    </label>
  )
}

function QosSelect({ value, onChange }: { value: 0 | 1 | 2; onChange: (q: 0 | 1 | 2) => void }) {
  return (
    <label className="text-sm">
      <span className="text-slate-500">QoS </span>
      <select className="rounded border border-slate-300 bg-transparent px-1 py-1 dark:border-slate-700" value={value} onChange={(e) => onChange(Number(e.target.value) as 0 | 1 | 2)}>
        <option value={0}>0</option>
        <option value={1}>1</option>
        <option value={2}>2</option>
      </select>
    </label>
  )
}

function Button({ disabled, children }: { disabled: boolean; children: ReactNode }) {
  return (
    <button disabled={disabled} className="rounded bg-slate-900 px-3 py-1 text-sm text-white disabled:opacity-40 dark:bg-slate-100 dark:text-slate-900">
      {children}
    </button>
  )
}

function Dot({ on }: { on: boolean }) {
  return <span className={`inline-block h-2 w-2 rounded-full ${on ? 'bg-green-500' : 'bg-slate-400'}`} />
}

function Muted({ children }: { children: ReactNode }) {
  return <p className="text-sm text-slate-500">{children}</p>
}
