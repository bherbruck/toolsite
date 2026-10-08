// DuckDB in the browser, loaded from the app's own files: the wasm and its
// worker are imported with ?url, so Vite copies them into the build and the
// page fetches them from /p/<app>/assets/, never from a CDN.
//
// DuckDB never reads a URL here. The page fetches a file through the app's
// handler, which decides who may have it, and hands DuckDB the bytes with
// registerFileBuffer. There is no httpfs, no read_parquet('<url>') and no
// ATTACH '<url>'.

import * as duckdb from '@duckdb/duckdb-wasm'
import wasmUrl from '@duckdb/duckdb-wasm/dist/duckdb-eh.wasm?url'
import workerUrl from '@duckdb/duckdb-wasm/dist/duckdb-browser-eh.worker.js?url'

let ready: Promise<duckdb.AsyncDuckDB> | null = null

/** One database for the page, started on first use. Only the exception
 * handling build ships: every current browser runs it, and it halves the
 * bundle. */
export function database(): Promise<duckdb.AsyncDuckDB> {
  ready ??= (async () => {
    const worker = new Worker(workerUrl)
    const db = new duckdb.AsyncDuckDB(new duckdb.VoidLogger(), worker)
    await db.instantiate(wasmUrl)
    return db
  })()
  return ready
}

export type Loaded = { bytes: number; fetchMs: number; registerMs: number }

/** Fetches one file through the handler and registers it under `name`.
 * The handler answers with the whole file or with a refusal; either way it
 * ran first. */
export async function loadFile(url: string, name: string): Promise<Loaded> {
  const db = await database()
  const started = performance.now()
  const res = await fetch(url)
  if (!res.ok) {
    let message = `HTTP ${res.status}`
    try {
      message = (await res.json()).error ?? message
    } catch {
      // not JSON: keep the status
    }
    throw new Error(message)
  }
  const buffer = new Uint8Array(await res.arrayBuffer())
  // Read before registering: the buffer is handed to DuckDB's worker, and
  // is empty on this side afterwards.
  const size = buffer.byteLength
  const fetched = performance.now()
  await db.registerFileBuffer(name, buffer)
  return { bytes: size, fetchMs: fetched - started, registerMs: performance.now() - fetched }
}

export type Rows = Record<string, unknown>[]

/** Runs one query and returns plain rows, numbers as numbers. */
export async function query(sql: string, params: unknown[] = []): Promise<{ rows: Rows; ms: number }> {
  const db = await database()
  const conn = await db.connect()
  const started = performance.now()
  try {
    const result = params.length
      ? await (await conn.prepare(sql)).query(...params)
      : await conn.query(sql)
    const rows = result.toArray().map((row) => {
      const plain: Record<string, unknown> = {}
      for (const [k, v] of Object.entries(row.toJSON())) plain[k] = typeof v === 'bigint' ? Number(v) : v
      return plain
    })
    return { rows, ms: performance.now() - started }
  } finally {
    await conn.close()
  }
}
