import { useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from 'react'
import { api, connectorUrl, type Card, type Lane, type Me, type Person } from './api'
import { socketUrl, useLive, type Status } from './live'

const LANES: { id: Lane; title: string }[] = [
  { id: 'todo', title: 'To do' },
  { id: 'doing', title: 'Doing' },
  { id: 'done', title: 'Done' },
]

const NAME_KEY = 'live-board:name'

function savedName(): string {
  try {
    return localStorage.getItem(NAME_KEY) ?? ''
  } catch {
    return ''
  }
}

type Note = { id: number; text: string }

export default function App() {
  const [cards, setCards] = useState<Card[]>([])
  const [people, setPeople] = useState<Person[]>([])
  const [me, setMe] = useState<Me | null>(null)
  const [notes, setNotes] = useState<Note[]>([])
  const [view, setView] = useState<'board' | 'connect'>('board')
  const [menu, setMenu] = useState(false)
  const menuRef = useRef<HTMLDivElement>(null)

  const note = (text: string) => {
    const id = Date.now() + Math.random()
    setNotes((n) => [...n, { id, text }])
    setTimeout(() => setNotes((n) => n.filter((x) => x.id !== id)), 6000)
  }

  // The name goes in the URL once, so the server can show it from the
  // first presence message. Later changes go over the socket.
  const url = useMemo(() => socketUrl('live/ws', { name: savedName() }), [])
  const live = useLive(url, (msg) => {
    switch (msg.type) {
      case 'board':
        setCards(msg.cards)
        setMe(msg.me)
        break
      case 'card':
        setCards((cs) => {
          const rest = cs.filter((c) => c.id !== msg.card.id)
          return [...rest, msg.card].sort((a, b) => a.created_at - b.created_at || a.id - b.id)
        })
        break
      case 'deleted':
        setCards((cs) => cs.filter((c) => c.id !== msg.id))
        break
      case 'who':
        setPeople(msg.people)
        break
      case 'nudge':
        note(`${msg.from} nudged you: ${msg.text}`)
        break
      case 'nudged':
        if (msg.reached === 0) note('Nobody received that nudge.')
        break
      case 'error':
        note(msg.error)
        break
    }
  })

  useEffect(() => {
    const close = (e: MouseEvent) => menuRef.current && !menuRef.current.contains(e.target as Node) && setMenu(false)
    document.addEventListener('mousedown', close)
    return () => document.removeEventListener('mousedown', close)
  }, [])

  // Changes go through the API. The handler writes them, then publishes to
  // every open board, this one included, so the screen updates from the
  // socket rather than from the response.
  const run = async (work: Promise<unknown>) => {
    try {
      await work
    } catch (e: any) {
      note(e.message)
    }
  }
  const add = (title: string, lane: Lane) => run(api('api/cards', { body: { title, lane } }))
  const move = (id: number, lane: Lane) => run(api(`api/cards/${id}/move`, { body: { lane } }))
  const remove = (id: number) => run(api(`api/cards/${id}`, { method: 'DELETE' }))

  const rename = () => {
    setMenu(false)
    const name = window.prompt('Display name', me?.name ?? '')?.trim()
    if (!name) return
    try {
      localStorage.setItem(NAME_KEY, name)
    } catch {
      // Private window: the name lasts until the page closes.
    }
    live.send({ type: 'name', name })
    setMe((m) => (m ? { ...m, name } : m))
  }

  const nudge = (person: Person) => {
    if (!live.send({ type: 'nudge', to: person.id, text: 'Look at the board.' })) note('Not connected.')
    else note(`Nudged ${person.name}.`)
  }

  return (
    <div className="min-h-screen bg-neutral-50 text-neutral-900 dark:bg-neutral-950 dark:text-neutral-100">
      <header className="flex flex-wrap items-center justify-between gap-3 border-b border-neutral-500/15 px-4 py-3 sm:px-6">
        <div className="flex items-center gap-3">
          <div className="flex items-center gap-2 font-semibold">
            <span className="text-xl">🗂️</span> Live board
          </div>
          <StatusPill status={live.status} />
        </div>
        <div className="flex items-center gap-3 text-sm text-neutral-500">
          <Presence people={people} me={me} onNudge={nudge} />
          <div className="relative" ref={menuRef}>
            <button onClick={() => setMenu(!menu)} className="rounded-md px-2 py-1 text-lg leading-none hover:bg-neutral-500/10" aria-label="Menu">
              ⋯
            </button>
            {menu && (
              <div className="absolute right-0 z-10 mt-1 w-60 rounded-lg border border-neutral-500/20 bg-white p-1 text-neutral-900 shadow-lg dark:bg-neutral-900 dark:text-neutral-100">
                {me && <div className="px-3 py-2 text-xs text-neutral-500">{me.email}</div>}
                <MenuItem onClick={() => (setView('board'), setMenu(false))}>Board</MenuItem>
                <MenuItem onClick={rename}>Change display name</MenuItem>
                <MenuItem onClick={() => (setView('connect'), setMenu(false))}>Connect an AI assistant</MenuItem>
              </div>
            )}
          </div>
        </div>
      </header>

      {view === 'connect' ? (
        <Connect />
      ) : (
        <main className="mx-auto grid max-w-6xl gap-4 px-4 py-6 sm:px-6 md:grid-cols-3">
          {LANES.map((lane) => (
            <LaneColumn
              key={lane.id}
              lane={lane}
              cards={cards.filter((c) => c.lane === lane.id)}
              onAdd={(title) => add(title, lane.id)}
              onMove={move}
              onDelete={remove}
            />
          ))}
        </main>
      )}

      <div className="pointer-events-none fixed inset-x-0 bottom-4 flex flex-col items-center gap-2 px-4">
        {notes.map((n) => (
          <div key={n.id} className="pointer-events-auto rounded-lg bg-neutral-900 px-4 py-2 text-sm text-white shadow-lg dark:bg-white dark:text-neutral-900">
            {n.text}
          </div>
        ))}
      </div>
    </div>
  )
}

function StatusPill({ status }: { status: Status }) {
  const look = {
    open: ['bg-emerald-500', 'Live'],
    connecting: ['bg-amber-500', 'Connecting'],
    closed: ['bg-red-500', 'Reconnecting'],
  }[status]
  return (
    <span className="flex items-center gap-1.5 rounded-full border border-neutral-500/20 px-2 py-0.5 text-xs text-neutral-500">
      <span className={`h-2 w-2 rounded-full ${look[0]}`} />
      {look[1]}
    </span>
  )
}

function initials(name: string): string {
  const parts = name.replace(/@.*/, '').split(/[\s._-]+/).filter(Boolean)
  return (parts.length > 1 ? parts[0][0] + parts[1][0] : name.slice(0, 1)).toUpperCase()
}

const HUES = ['bg-sky-600', 'bg-violet-600', 'bg-rose-600', 'bg-teal-600', 'bg-orange-600', 'bg-indigo-600']

function hue(id: string): string {
  let h = 0
  for (const ch of id) h = (h * 31 + ch.charCodeAt(0)) >>> 0
  return HUES[h % HUES.length]
}

/** Who has the board open. Click someone else to nudge them. */
function Presence({ people, me, onNudge }: { people: Person[]; me: Me | null; onNudge: (p: Person) => void }) {
  return (
    <div className="flex items-center gap-1" aria-label="People on the board">
      {people.map((p) => {
        const self = p.id === me?.id
        return (
          <button
            key={p.id}
            disabled={self}
            onClick={() => onNudge(p)}
            title={self ? `${p.name} (you)` : `Nudge ${p.name}`}
            className={`flex h-8 w-8 items-center justify-center rounded-full text-xs font-semibold text-white ring-2 ring-neutral-50 dark:ring-neutral-950 ${hue(p.id)} ${
              self ? 'cursor-default' : 'hover:scale-110'
            }`}
          >
            {initials(p.name)}
          </button>
        )
      })}
      <span className="ml-1 hidden text-xs sm:inline">{people.length === 1 ? '1 person here' : `${people.length} people here`}</span>
    </div>
  )
}

function LaneColumn({
  lane,
  cards,
  onAdd,
  onMove,
  onDelete,
}: {
  lane: { id: Lane; title: string }
  cards: Card[]
  onAdd: (title: string) => void
  onMove: (id: number, lane: Lane) => void
  onDelete: (id: number) => void
}) {
  const [title, setTitle] = useState('')
  const [over, setOver] = useState(false)
  const submit = (e: FormEvent) => {
    e.preventDefault()
    if (!title.trim()) return
    onAdd(title.trim())
    setTitle('')
  }
  const index = LANES.findIndex((l) => l.id === lane.id)
  return (
    <section
      onDragOver={(e) => (e.preventDefault(), setOver(true))}
      onDragLeave={() => setOver(false)}
      onDrop={(e) => {
        e.preventDefault()
        setOver(false)
        const id = Number(e.dataTransfer.getData('text/plain'))
        if (id) onMove(id, lane.id)
      }}
      className={`flex flex-col rounded-xl border p-3 transition-colors ${
        over ? 'border-sky-500 bg-sky-500/5' : 'border-neutral-500/20 bg-white/60 dark:bg-neutral-900/60'
      }`}
    >
      <h2 className="mb-3 flex items-center justify-between text-sm font-semibold uppercase tracking-wide text-neutral-500">
        {lane.title}
        <span className="rounded-full bg-neutral-500/10 px-2 text-xs">{cards.length}</span>
      </h2>
      <ul className="flex-1 space-y-2">
        {cards.map((card) => (
          <li
            key={card.id}
            draggable
            onDragStart={(e) => e.dataTransfer.setData('text/plain', String(card.id))}
            className="group rounded-lg border border-neutral-500/20 bg-white p-3 shadow-sm dark:bg-neutral-900"
          >
            <p className="text-sm">{card.title}</p>
            <div className="mt-2 flex items-center justify-between text-xs text-neutral-500">
              <span className="truncate">{card.author_email ?? 'an assistant'}</span>
              <span className="flex gap-1 opacity-60 group-hover:opacity-100">
                {index > 0 && <Small onClick={() => onMove(card.id, LANES[index - 1].id)} label="Move left">←</Small>}
                {index < LANES.length - 1 && <Small onClick={() => onMove(card.id, LANES[index + 1].id)} label="Move right">→</Small>}
                <Small onClick={() => onDelete(card.id)} label="Delete">✕</Small>
              </span>
            </div>
          </li>
        ))}
      </ul>
      <form onSubmit={submit} className="mt-3">
        <input
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          placeholder="Add a card"
          maxLength={200}
          className="w-full rounded-md border border-neutral-500/30 bg-transparent px-3 py-1.5 text-sm outline-none focus:border-sky-500"
        />
      </form>
    </section>
  )
}

function Small({ onClick, label, children }: { onClick: () => void; label: string; children: ReactNode }) {
  return (
    <button onClick={onClick} aria-label={label} title={label} className="rounded px-1.5 py-0.5 hover:bg-neutral-500/15">
      {children}
    </button>
  )
}

function MenuItem({ onClick, children }: { onClick: () => void; children: ReactNode }) {
  return (
    <button onClick={onClick} className="block w-full rounded-md px-3 py-2 text-left hover:bg-neutral-500/10">
      {children}
    </button>
  )
}

function Connect() {
  const [copied, setCopied] = useState(false)
  const link = connectorUrl()
  return (
    <div className="mx-auto max-w-2xl px-6 py-8">
      <h1 className="text-xl font-semibold">Connect an AI assistant</h1>
      <p className="mt-1 text-sm text-neutral-500">
        Add this link as a connector in Claude or ChatGPT and sign in with your account. The assistant can then add cards to
        this board, as you. Everyone with the board open sees each card at once.
      </p>
      <div className="mt-5 flex items-center gap-2">
        <input
          readOnly
          value={link}
          onFocus={(e) => e.target.select()}
          className="flex-1 rounded-md border border-neutral-500/30 bg-transparent px-3 py-1.5 font-mono text-sm"
        />
        <button
          onClick={() =>
            navigator.clipboard?.writeText(link).then(() => {
              setCopied(true)
              setTimeout(() => setCopied(false), 1500)
            })
          }
          className="rounded-md bg-neutral-900 px-3 py-1.5 text-sm font-medium text-white dark:bg-white dark:text-neutral-900"
        >
          {copied ? 'Copied' : 'Copy'}
        </button>
      </div>
      <ul className="mt-5 space-y-1 text-sm">
        <li>
          <b>add_card</b>: add a card to a lane (todo, doing or done)
        </li>
      </ul>
    </div>
  )
}
