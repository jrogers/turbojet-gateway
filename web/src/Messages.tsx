import { useRef, useState, type FormEvent } from 'react'
import * as api from './api.ts'
import MessageList from './MessageList.tsx'
import { useLive } from './store.ts'
import type { Me } from './types.ts'

/** `api::messages::RANGE_MAX`: the longest range a search covers, in ms. */
export const RANGE_MAX = 86_400_000

/** `api::messages::PAGE_MAX`: the most hits a page holds. */
export const PAGE_MAX = 1_000

/** `api::messages::CONDITIONS_MAX`: the sessions, MsgTypes and tags a search gives at most, together. */
export const CONDITIONS_MAX = 100

/** The search form as typed. The times are `datetime-local` values, read as UTC. */
export interface Search {
  session: string
  /** `''` for either, `in` or `out`. */
  direction: string
  /** Comma-separated. */
  msgTypes: string
  /** `[tag, value]` rows: every one must hold. Rows left empty are ignored. */
  tags: [string, string][]
  since: string
  /** `''` for a day after `since`. */
  until: string
  /** `''` for the API's default. */
  limit: string
}

/** A `datetime-local` value (`2026-10-14T09:30`, maybe with seconds) as UTC, in ms; NaN if not one. */
function utcTime(value: string): number {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?$/.test(value)) return NaN
  // With an explicit Z, never the browser's zone.
  return Date.parse(`${value}Z`)
}

/**
 * `search`'s query for `GET /api/messages`, as `api::messages::parse` takes it: `session`, one
 * `tag` per condition, the MsgTypes as one `msg_type` list, and `since` and `until` as UTC RFC 3339.
 * Or why it can't be asked: a range the API refuses, a tag row that isn't TAG and VALUE, too many
 * conditions, a limit out of bounds.
 */
export function searchParams(search: Search): [string, string][] | string {
  if (search.since === '') return 'Since is required.'
  const since = utcTime(search.since)
  const until = search.until === '' ? since + RANGE_MAX : utcTime(search.until)
  if (Number.isNaN(since) || Number.isNaN(until)) return 'Since and until are a date and a time.'
  if (until <= since) return 'Until must be after since.'
  if (until - since > RANGE_MAX) return 'A search covers 24 hours at most.'

  const session = search.session.trim()
  const msgTypes = search.msgTypes
    .split(',')
    .map((type) => type.trim())
    .filter((type) => type !== '')
  const tags: string[] = []
  for (const [tag, value] of search.tags) {
    // The value as typed: it's matched exactly.
    const number = tag.trim()
    if (number === '' && value === '') continue
    if (!/^\d{1,9}$/.test(number) || Number(number) === 0 || value === '') {
      return `Tag condition ${number}=${value} needs a tag number and a value.`
    }
    tags.push(`${number}=${value}`)
  }
  if ((session === '' ? 0 : 1) + msgTypes.length + tags.length > CONDITIONS_MAX) {
    return `A search takes ${CONDITIONS_MAX} sessions, MsgTypes and tags at most.`
  }
  const limit = search.limit.trim()
  if (limit !== '' && !(/^\d+$/.test(limit) && Number(limit) >= 1 && Number(limit) <= PAGE_MAX)) {
    return `Limit is 1 to ${PAGE_MAX}.`
  }

  const params: [string, string][] = []
  if (session !== '') params.push(['session', session])
  if (search.direction !== '') params.push(['direction', search.direction])
  if (msgTypes.length > 0) params.push(['msg_type', msgTypes.join(',')])
  for (const tag of tags) params.push(['tag', tag])
  params.push(['since', new Date(since).toISOString()], ['until', new Date(until).toISOString()])
  if (limit !== '') params.push(['limit', limit])
  return params
}

/** Today's UTC midnight as a `datetime-local` value. */
const today = () => `${new Date().toISOString().slice(0, 10)}T00:00`

/**
 * The message log, searched: by session, direction, MsgTypes and tag conditions, over a day at
 * most. A scoped user's search finds only its clients' sessions, as the API narrows it; naming
 * another's is refused, with the API's text. Searched again if a snapshot changes the user's
 * clients.
 */
export default function Messages({ me }: { me: Me }) {
  const live = useLive()
  // `n` counts the searches asked, so asking the same again reads it afresh.
  const [search, setSearch] = useState<{ params: [string, string][]; n: number }>()
  const [invalid, setInvalid] = useState<string>()
  // The tag rows' keys: rows are added and removed, so not their positions.
  const [rows, setRows] = useState<number[]>([0])
  const nextRow = useRef(1)
  // Suggestions: a scoped user may search only its clients' sessions. Past ones can be typed.
  const sessions = live.sessions.filter((session) => me.clients === null || session.side === 'client')

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = new FormData(event.currentTarget)
    const text = (name: string) => String(form.get(name) ?? '')
    const values = form.getAll('value').map(String)
    const params = searchParams({
      session: text('session'),
      direction: text('direction'),
      msgTypes: text('msg_type'),
      tags: form.getAll('tag').map((tag, i) => [String(tag), values[i] ?? '']),
      since: text('since'),
      until: text('until'),
      limit: text('limit'),
    })
    if (typeof params === 'string') {
      setInvalid(params)
      setSearch(undefined)
    } else {
      setInvalid(undefined)
      setSearch((before) => ({ params, n: (before?.n ?? 0) + 1 }))
    }
  }

  const addRow = () => {
    setRows((rows) => [...rows, nextRow.current])
    nextRow.current += 1
  }

  return (
    <>
      <h1>Messages</h1>
      <form className="filters" onSubmit={submit}>
        <label>
          Session
          <input name="session" list="known-sessions" placeholder="FIX.4.4:JETBRIDGE->…" />
        </label>
        <datalist id="known-sessions">
          {sessions.map((session) => (
            <option key={`${session.side} ${session.id}`} value={session.id} />
          ))}
        </datalist>
        <label>
          Direction
          <select name="direction" defaultValue="">
            <option value="">any</option>
            <option value="in">in</option>
            <option value="out">out</option>
          </select>
        </label>
        <label>
          MsgType
          <input name="msg_type" placeholder="D,F" />
        </label>
        <label>
          Since (UTC)
          <input type="datetime-local" step="1" name="since" defaultValue={today()} required />
        </label>
        <label>
          Until (UTC; a day after since if empty)
          <input type="datetime-local" step="1" name="until" />
        </label>
        <label>
          Limit
          <input type="number" name="limit" min="1" max={PAGE_MAX} placeholder="100" />
        </label>
        <fieldset>
          <legend>Tag conditions, all of them</legend>
          {rows.map((row, at) => (
            <div key={row} className="filters">
              <label>
                Tag
                <input name="tag" inputMode="numeric" placeholder="11" />
              </label>
              <label>
                Value
                <input name="value" />
              </label>
              <button
                type="button"
                aria-label={`Remove tag condition ${at + 1}`}
                onClick={() => setRows((rows) => rows.filter((r) => r !== row))}
              >
                Remove
              </button>
            </div>
          ))}
          <button type="button" onClick={addRow} disabled={rows.length >= CONDITIONS_MAX}>
            Add tag condition
          </button>
        </fieldset>
        <button type="submit" className="primary">
          Search
        </button>
      </form>
      {invalid && (
        <p className="error" role="alert">
          {invalid}
        </p>
      )}
      {search && (
        <MessageList
          key={`${search.n} ${JSON.stringify(live.clients)}`}
          fetchPage={(after) => api.messages(after === null ? search.params : [...search.params, ['after', after]])}
        />
      )}
    </>
  )
}
