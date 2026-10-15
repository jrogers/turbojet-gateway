import { useEffect, useRef, useState } from 'react'
import * as api from './api.ts'
import { utc } from './time.ts'
import type { Hit, Page } from './types.ts'

/**
 * Messages from the log, oldest first, a page at a time from `fetchPage(after)`: "More" reads on
 * from `next`; at the end, while the range is still to come, "Check for newer" reads on from
 * `follow`. Each shows as plain text, never as HTML: the wire is the counterparties' to write. It
 * reads its first page once, on mounting: key it by what it searches, so a new search starts
 * afresh. A 403 (rescoped since) or 404 (no message log, or no such order) clears what was shown,
 * and Retry starts again from the first page. Keeping the hits and clearing only the paging would
 * show the first page twice, as Retry reads it again onto them.
 */
export default function MessageList({ fetchPage }: { fetchPage: (after: string | null) => Promise<Page> }) {
  const [hits, setHits] = useState<Hit[]>([])
  const [page, setPage] = useState<{ next: string | null; follow: string | null }>()
  const [busy, setBusy] = useState(true)
  const [error, setError] = useState<string>()
  // A token for the mounting: a read answering after it's gone is dropped (StrictMode mounts
  // twice, and the first's answer mustn't be shown twice).
  const reading = useRef<object>(undefined)

  async function read(after: string | null, at = reading.current) {
    setBusy(true)
    setError(undefined)
    try {
      const answer = await fetchPage(after)
      if (reading.current !== at) return
      setHits((hits) => [...hits, ...answer.messages])
      setPage({ next: answer.next, follow: answer.follow })
    } catch (e) {
      if (reading.current !== at) return
      if (e instanceof api.ApiError && (e.status === 403 || e.status === 404)) {
        setHits([])
        setPage(undefined)
      }
      setError(api.message(e))
    } finally {
      if (reading.current === at) setBusy(false)
    }
  }

  // Once: keyed by its search.
  useEffect(() => {
    const at = {}
    reading.current = at
    void read(null, at)
    return () => {
      reading.current = undefined
    }
  }, [])

  return (
    <>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      {hits.length > 0 && (
        <div className="table-wrap">
          <table className="messages">
            <thead>
              <tr>
                <th>Time (UTC)</th>
                <th>Direction</th>
                <th>Session</th>
                <th>MsgType</th>
                <th>Message</th>
              </tr>
            </thead>
            <tbody>
              {hits.map((hit, at) => (
                <tr key={at}>
                  <td>
                    <time dateTime={hit.time}>{utc(hit.time, 3)}</time>
                  </td>
                  <td>
                    <span className={hit.direction === 'in' ? 'pill info' : 'pill muted'}>{hit.direction}</span>
                  </td>
                  <td>{hit.session ?? ''}</td>
                  <td>{hit.msg_type ?? ''}</td>
                  <td>
                    <details>
                      <summary>
                        <code className="raw">{hit.raw}</code>
                      </summary>
                      <table className="tags">
                        <thead className="visually-hidden">
                          <tr>
                            <th>Tag</th>
                            <th>Value</th>
                          </tr>
                        </thead>
                        <tbody>
                          {hit.fields.map(([tag, value], i) => (
                            <tr key={i}>
                              <td className="num">{tag}</td>
                              <td>{value}</td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </details>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <p className="muted">
        {busy ? 'Reading…' : hits.length === 0 && !error ? 'No messages.' : null}{' '}
        {page === undefined && error && !busy && (
          <button type="button" onClick={() => read(null)}>
            Retry
          </button>
        )}
        {page?.next != null ? (
          <button type="button" onClick={() => read(page.next)} disabled={busy}>
            More
          </button>
        ) : (
          page?.follow != null && (
            <button type="button" onClick={() => read(page.follow)} disabled={busy}>
              Check for newer
            </button>
          )
        )}
      </p>
    </>
  )
}
