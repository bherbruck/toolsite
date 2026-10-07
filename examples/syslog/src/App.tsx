import { useEffect, useState } from 'react'

// Every URL here is relative: the app is served from /p/<slug>/.

type Log = {
  id: number
  received_at: number
  remote: string
  host: string
  app: string | null
  facility: number | null
  severity: number | null
  message: string
}

const SEVERITIES = ['emergency', 'alert', 'critical', 'error', 'warning', 'notice', 'info', 'debug']
const COLORS = [
  'text-red-600', 'text-red-600', 'text-red-600', 'text-red-500',
  'text-amber-500', 'text-sky-500', 'text-gray-500', 'text-gray-400',
]
const SHOWN = 500

export default function App() {
  const [logs, setLogs] = useState<Log[]>([])
  const [hosts, setHosts] = useState<string[]>([])
  const [severity, setSeverity] = useState('')
  const [host, setHost] = useState('')
  const [live, setLive] = useState(false)

  // The stored rows for the filters, newest first.
  useEffect(() => {
    const query = new URLSearchParams({ severity, host })
    fetch(`api/logs?${query}`).then((r) => r.json()).then((d) => setLogs(d.logs ?? []))
    fetch('api/hosts').then((r) => r.json()).then((d) => setHosts(d.hosts ?? []))
  }, [severity, host])

  // The live tail: each stored line arrives on the socket as JSON. A line
  // the filters leave out is not shown. The socket reconnects after a
  // drop, waiting longer after each failure.
  useEffect(() => {
    let stopped = false
    let failures = 0
    let ws: WebSocket | undefined
    let retry: ReturnType<typeof setTimeout> | undefined
    const open = () => {
      const url = new URL('tail', document.baseURI)
      url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:'
      ws = new WebSocket(url)
      ws.onopen = () => {
        failures = 0
        setLive(true)
      }
      ws.onmessage = (e) => {
        const log: Log = JSON.parse(e.data)
        if (host && log.host !== host) return
        if (severity && (log.severity === null || log.severity > Number(severity))) return
        setLogs((shown) => [log, ...shown].slice(0, SHOWN))
        setHosts((known) => (known.includes(log.host) ? known : [...known, log.host].sort()))
      }
      ws.onclose = () => {
        setLive(false)
        if (stopped) return
        retry = setTimeout(open, Math.min(15_000, 500 * 2 ** failures++))
      }
    }
    open()
    return () => {
      stopped = true
      clearTimeout(retry)
      ws?.close()
    }
  }, [severity, host])

  return (
    <div className="mx-auto max-w-6xl p-4 font-sans">
      <header className="mb-4 flex flex-wrap items-center gap-3">
        <h1 className="mr-auto text-xl font-semibold">Syslog</h1>
        <span className={live ? 'text-green-600' : 'text-gray-400'}>{live ? '● live' : '○ reconnecting'}</span>
        <select className="rounded border px-2 py-1" value={severity} onChange={(e) => setSeverity(e.target.value)}>
          <option value="">Every severity</option>
          {SEVERITIES.map((name, n) => (
            <option key={n} value={n}>{n === 0 ? name : `${name} and worse`}</option>
          ))}
        </select>
        <select className="rounded border px-2 py-1" value={host} onChange={(e) => setHost(e.target.value)}>
          <option value="">Every host</option>
          {hosts.map((h) => <option key={h}>{h}</option>)}
        </select>
      </header>
      {logs.length === 0 && <p className="text-gray-500">Nothing received yet. See the README to send a test line.</p>}
      <table className="w-full text-left font-mono text-sm">
        <tbody>
          {logs.map((log) => (
            <tr key={log.id} className="border-b border-gray-500/20 align-top">
              <td className="whitespace-nowrap pr-3 text-gray-500">{new Date(log.received_at * 1000).toLocaleString()}</td>
              <td className={`pr-3 ${log.severity === null ? 'text-gray-400' : COLORS[log.severity]}`}>
                {log.severity === null ? 'unknown' : SEVERITIES[log.severity]}
              </td>
              <td className="pr-3">{log.host}</td>
              <td className="pr-3 text-gray-500">{log.app ?? ''}</td>
              <td className="whitespace-pre-wrap break-all">{log.message}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
