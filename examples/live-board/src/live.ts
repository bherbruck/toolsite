import { useEffect, useRef, useState } from 'react'

// One WebSocket to the app's declared socket, kept open for as long as the
// page is. toolsite holds the server end; the handler gets each message as
// an event and answers with send or publish.
//
// It reconnects on its own, waiting longer after each failure (0.5 s, 1 s,
// 2 s, up to 15 s, with jitter so a restarted server is not hit by every
// browser at once). The server sends the whole board as the first message
// of every connection, so a reconnect is also a refetch: nothing missed
// while the socket was down needs replaying.

export type Status = 'connecting' | 'open' | 'closed'

export function socketUrl(path: string, params: Record<string, string> = {}): string {
  // Relative to the app's base, so it is right under /p/<slug>/ on any site.
  const url = new URL(path, document.baseURI)
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:'
  for (const [k, v] of Object.entries(params)) if (v) url.searchParams.set(k, v)
  return url.href
}

export function useLive(url: string, onMessage: (msg: any) => void) {
  const [status, setStatus] = useState<Status>('connecting')
  const socket = useRef<WebSocket | null>(null)
  const handler = useRef(onMessage)
  handler.current = onMessage

  useEffect(() => {
    let stopped = false
    let failures = 0
    let retry: ReturnType<typeof setTimeout> | undefined
    let ping: ReturnType<typeof setInterval> | undefined

    const open = () => {
      setStatus('connecting')
      const ws = new WebSocket(url)
      socket.current = ws
      ws.onopen = () => {
        failures = 0
        setStatus('open')
        // Keeps this person on the presence list.
        ping = setInterval(() => ws.readyState === WebSocket.OPEN && ws.send('{"type":"ping"}'), 25_000)
      }
      ws.onmessage = (e) => {
        try {
          handler.current(JSON.parse(e.data))
        } catch {
          // Not JSON: not from this app's handler.
        }
      }
      ws.onclose = () => {
        clearInterval(ping)
        if (socket.current === ws) socket.current = null
        if (stopped) return
        setStatus('closed')
        const wait = Math.min(15_000, 500 * 2 ** failures) * (0.75 + Math.random() / 2)
        failures += 1
        retry = setTimeout(open, wait)
      }
    }
    open()

    // A tab coming back from sleep reconnects at once rather than waiting.
    const wake = () => {
      if (document.visibilityState === 'visible' && !socket.current) {
        clearTimeout(retry)
        failures = 0
        open()
      }
    }
    document.addEventListener('visibilitychange', wake)

    return () => {
      stopped = true
      clearTimeout(retry)
      clearInterval(ping)
      document.removeEventListener('visibilitychange', wake)
      socket.current?.close()
    }
  }, [url])

  const send = (msg: unknown) => {
    const ws = socket.current
    if (ws?.readyState !== WebSocket.OPEN) return false
    ws.send(JSON.stringify(msg))
    return true
  }
  return { status, send }
}
