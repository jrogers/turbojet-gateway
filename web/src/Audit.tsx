import { useEffect, useState, type FormEvent } from 'react'
import * as api from './api.ts'
import { utc, utcTime } from './time.ts'
import type { AuditEntry } from './types.ts'

/** `audit::MAX_READ`: the most entries a read returns. */
export const AUDIT_MAX = 1_000

/** `api::AUDIT_READ_DEFAULT`. */
const AUDIT_DEFAULT = 100

const message = (e: unknown) => (e instanceof Error ? e.message : String(e))

/**
 * The audit log [unscoped admin], newest first: the last `limit` entries from `since`, as
 * `GET /api/audit` reads it. It can't page further back, so a full read says older ones are left
 * out, and a larger limit (up to 1,000) reaches them.
 */
export default function Audit() {
  // `n` counts the reads asked, so asking the same again reads it afresh.
  const [query, setQuery] = useState({ since: '', limit: AUDIT_DEFAULT, n: 0 })
  // `undefined` while read; `null` after a refusal or failure, with `error` saying why.
  const [entries, setEntries] = useState<AuditEntry[] | null>()
  const [error, setError] = useState<string>()
  const [invalid, setInvalid] = useState<string>()

  useEffect(() => {
    let current = true
    api.audit(query.since, query.limit).then(
      (entries) => {
        if (!current) return
        setEntries(entries.toReversed())
        setError(undefined)
      },
      (e: unknown) => {
        if (!current) return
        setEntries(null)
        setError(message(e))
      },
    )
    return () => {
      current = false
    }
  }, [query])

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = new FormData(event.currentTarget)
    const sinceText = String(form.get('since') ?? '')
    const limitText = String(form.get('limit') ?? '').trim()
    const since = sinceText === '' ? '' : utcTime(sinceText)
    const limit = limitText === '' ? AUDIT_DEFAULT : Number(limitText)
    if (Number.isNaN(since)) return setInvalid('Since is a date and a time.')
    if (!Number.isInteger(limit) || limit < 1 || limit > AUDIT_MAX) return setInvalid(`Limit is 1 to ${AUDIT_MAX}.`)
    setInvalid(undefined)
    setEntries(undefined)
    setQuery((before) => ({ since: since === '' ? '' : new Date(since).toISOString(), limit, n: before.n + 1 }))
  }

  return (
    <>
      <h1 tabIndex={-1}>Audit</h1>
      <form className="filters" onSubmit={submit}>
        <label>
          Since (UTC; the start if empty)
          <input type="datetime-local" step="1" name="since" />
        </label>
        <label>
          Limit
          <input type="number" name="limit" min="1" max={AUDIT_MAX} placeholder={String(AUDIT_DEFAULT)} />
        </label>
        <button type="submit" className="primary">
          Read
        </button>
      </form>
      {(invalid ?? error) && (
        <p className="error" role="alert">
          {invalid ?? error}
        </p>
      )}
      {entries === undefined ? (
        <p className="muted">Loading…</p>
      ) : (
        entries !== null && <Entries entries={entries} limit={query.limit} />
      )}
    </>
  )
}

function Entries({ entries, limit }: { entries: AuditEntry[]; limit: number }) {
  if (entries.length === 0) return <p className="muted">None.</p>
  return (
    <>
      <p className="muted">
        {entries.length === limit
          ? `The last ${limit}, newest first: older ones are left out${limit < AUDIT_MAX ? '; a larger limit reaches them' : ''}.`
          : `${entries.length}, newest first.`}
      </p>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Time (UTC)</th>
              <th>User</th>
              <th>Role</th>
              <th>IP</th>
              <th>Action</th>
              <th>Target</th>
              <th>Params</th>
              <th>Reason</th>
              <th>Result</th>
            </tr>
          </thead>
          <tbody>
            {entries.map((entry, i) => (
              <tr key={i}>
                <td>{utc(entry.time)}</td>
                <td>{entry.user ?? ''}</td>
                <td>{entry.role ?? ''}</td>
                <td>{entry.ip}</td>
                <td>{entry.action}</td>
                <td>{entry.target ?? ''}</td>
                <td className="wrap">
                  <code className="raw">{entry.params === null ? '' : JSON.stringify(entry.params)}</code>
                </td>
                <td className="wrap">{entry.reason ?? ''}</td>
                <td>{entry.result}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  )
}
