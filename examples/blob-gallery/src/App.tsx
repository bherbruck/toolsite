import { useRef, useState } from 'react'
import { api, bytes, when } from './api'
import { Button, ErrorText, input, useLoad } from './ui'

type Photo = { id: number; caption: string; owner_id: string; owner_email: string; content_type: string; size: number; created_at: number }

/** Draws a thumbnail in the browser, so the server never decodes an image. */
async function thumbnail(file: File, longest = 480): Promise<Blob> {
  const bitmap = await createImageBitmap(file)
  const scale = Math.min(1, longest / Math.max(bitmap.width, bitmap.height))
  const canvas = document.createElement('canvas')
  canvas.width = Math.round(bitmap.width * scale)
  canvas.height = Math.round(bitmap.height * scale)
  canvas.getContext('2d')!.drawImage(bitmap, 0, 0, canvas.width, canvas.height)
  return new Promise((resolve, reject) => canvas.toBlob((b) => (b ? resolve(b) : reject(new Error('could not draw a thumbnail'))), 'image/jpeg', 0.82))
}

async function put(url: string, body: Blob, type: string) {
  const res = await fetch(url, { method: 'PUT', headers: { 'content-type': type }, body })
  if (!res.ok) throw new Error(`upload failed: HTTP ${res.status} ${await res.text()}`)
}

export default function App() {
  const me = useLoad(() => api('api/me'))
  const photos = useLoad<{ photos: Photo[] }>(() => api('api/photos'))
  const [caption, setCaption] = useState('')
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const picker = useRef<HTMLInputElement>(null)

  const add = async (files: FileList) => {
    setError(null)
    try {
      for (const [i, file] of Array.from(files).entries()) {
        if (!file.type.startsWith('image/')) throw new Error(`${file.name} is not an image`)
        setBusy(`Uploading ${i + 1} of ${files.length}`)
        // 1. A row and two upload URLs. 2. Both files straight to storage.
        // 3. The handler checks they arrived before the photo is listed.
        const ticket = await api('api/photos', { body: { caption: caption || file.name.replace(/\.[^.]+$/, '') } })
        await put(ticket.full, file, file.type)
        await put(ticket.thumb, await thumbnail(file), 'image/jpeg')
        await api(`api/photos/${ticket.id}/ready`, { body: {} })
      }
      setCaption('')
      photos.reload()
    } catch (err: any) {
      setError(err.message)
    } finally {
      setBusy(null)
      if (picker.current) picker.current.value = ''
    }
  }

  const remove = async (id: number) => {
    try {
      await api(`api/photos/${id}`, { method: 'DELETE' })
      photos.reload()
    } catch (err: any) {
      setError(err.message)
    }
  }

  const list = photos.data?.photos ?? []
  const mine = me.data?.user?.id

  return (
    <div className="min-h-screen bg-neutral-50 text-neutral-900 dark:bg-neutral-950 dark:text-neutral-100">
      <header className="border-b border-neutral-500/15 px-6 py-4">
        <div className="mx-auto flex max-w-6xl flex-wrap items-center justify-between gap-3">
          <div>
            <h1 className="flex items-center gap-2 text-xl font-semibold">
              <span>🖼️</span> Gallery
            </h1>
            <p className="text-sm text-neutral-500">
              Photos go from your browser straight to the app's file store. The handler only hands out the upload URLs.
            </p>
          </div>
          <div className="flex items-center gap-2">
            <input className={input} placeholder="Caption (optional)" value={caption} onChange={(e) => setCaption(e.target.value)} />
            <input ref={picker} type="file" accept="image/*" multiple hidden onChange={(e) => e.target.files?.length && add(e.target.files)} />
            <Button onClick={() => picker.current?.click()} disabled={!!busy}>
              {busy ?? 'Add photos'}
            </Button>
          </div>
        </div>
      </header>

      <main className="mx-auto max-w-6xl px-6 py-6">
        <ErrorText error={error ?? photos.error} />
        {list.length === 0 && !photos.error && (
          <div className="mt-10 rounded-xl border-2 border-dashed border-neutral-500/25 p-12 text-center text-neutral-500">
            No photos yet. Add some with the button above.
          </div>
        )}
        <ul className="mt-4 grid grid-cols-2 gap-4 sm:grid-cols-3 lg:grid-cols-4">
          {list.map((p) => (
            <li key={p.id} className="group overflow-hidden rounded-xl border border-neutral-500/20 bg-white shadow-sm dark:bg-neutral-900">
              <a href={`api/photos/${p.id}/full`} target="_blank" rel="noreferrer" className="block aspect-[4/3] overflow-hidden bg-neutral-500/10">
                <img src={`api/photos/${p.id}/thumb`} alt={p.caption} loading="lazy" className="h-full w-full object-cover transition group-hover:scale-105" />
              </a>
              <div className="flex items-start justify-between gap-2 p-3">
                <div className="min-w-0">
                  <div className="truncate text-sm font-medium">{p.caption || 'Untitled'}</div>
                  <div className="truncate text-xs text-neutral-500">
                    {p.owner_email} · {bytes(p.size)} · {when(p.created_at)}
                  </div>
                </div>
                {(p.owner_id === mine || me.data?.is_curator) && (
                  <button onClick={() => remove(p.id)} className="text-xs text-neutral-400 hover:text-red-500">
                    Remove
                  </button>
                )}
              </div>
            </li>
          ))}
        </ul>
      </main>
    </div>
  )
}
