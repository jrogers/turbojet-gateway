import { useEffect, useRef, useState, type FormEvent } from 'react'
import * as api from './api.ts'
import MessageList from './MessageList.tsx'
import { useLive } from './store.ts'
import { utcTime } from './time.ts'
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

/** `search`'s since and until in ms, until a day after since if empty, or why the API would refuse them. */
export function range(search: Pick<Search, 'since' | 'until'>): [number, number] | string {
  if (search.since === '') return 'Since is required.'
  const since = utcTime(search.since)
  const until = search.until === '' ? since + RANGE_MAX : utcTime(search.until)
  if (Number.isNaN(since) || Number.isNaN(until)) return 'Since and until are a date and a time.'
  if (until <= since) return 'Until must be after since.'
  if (until - since > RANGE_MAX) return 'A search covers 24 hours at most.'
  return [since, until]
}

/**
 * `search`'s query for `GET /api/messages`, as `api::messages::parse` takes it: `session`, one
 * `tag` per condition, the MsgTypes as one `msg_type` list, and `since` and `until` as UTC RFC 3339.
 * Or why it can't be asked: a range the API refuses, a tag row that isn't TAG and VALUE, too many
 * conditions, a limit out of bounds.
 */
export function searchParams(search: Search): [string, string][] | string {
  const times = range(search)
  if (typeof times === 'string') return times
  const [since, until] = times

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
 * another's is refused, with the API's text. Searched again if the user's clients change: `me`'s
 * are login's until a snapshot, then the snapshot's.
 */
export default function Messages({ me }: { me: Me }) {
  const live = useLive()
  // `n` counts the searches asked, so asking the same again reads it afresh.
  const [search, setSearch] = useState<{ params: [string, string][]; n: number }>()
  // Why the form can't be asked, and whether that's its range, which since and until then name.
  const [invalid, setInvalid] = useState<{ text: string; range: boolean }>()
  // The tag rows' keys: rows are added and removed, so not their positions.
  const [rows, setRows] = useState<number[]>([0])
  const nextRow = useRef(1)
  // Each row's Tag input and Remove button, by key, and the Add button: where focus goes as rows
  // come and go, once they're rendered.
  const tagInputs = useRef(new Map<number, HTMLInputElement>())
  const removes = useRef(new Map<number, HTMLButtonElement>())
  const addButton = useRef<HTMLButtonElement>(null)
  const focusNext = useRef<() => HTMLElement | null | undefined>(undefined)
  // Suggestions: a scoped user may search only its clients' sessions. Past ones can be typed.
  const sessions = live.sessions.filter((session) => me.clients === null || session.side === 'client')

  useEffect(() => {
    const target = focusNext.current?.()
    focusNext.current = undefined
    target?.focus()
  }, [rows])

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = new FormData(event.currentTarget)
    const text = (name: string) => String(form.get(name) ?? '')
    const values = form.getAll('value').map(String)
    const fields: Search = {
      session: text('session'),
      direction: text('direction'),
      msgTypes: text('msg_type'),
      tags: form.getAll('tag').map((tag, i) => [String(tag), values[i] ?? '']),
      since: text('since'),
      until: text('until'),
      limit: text('limit'),
    }
    const params = searchParams(fields)
    if (typeof params === 'string') {
      setInvalid({ text: params, range: typeof range(fields) === 'string' })
      // On purpose: results left under a form they no longer match would read as its answer.
      setSearch(undefined)
    } else {
      setInvalid(undefined)
      setSearch((before) => ({ params, n: (before?.n ?? 0) + 1 }))
    }
  }

  const addRow = () => {
    const row = nextRow.current
    nextRow.current += 1
    focusNext.current = () => tagInputs.current.get(row)
    setRows((rows) => [...rows, row])
  }

  // Focus to the row before's Remove, or with none, to Add: the button pressed has gone.
  const removeRow = (row: number, at: number) => {
    const before = rows[at - 1]
    focusNext.current = () => (before === undefined ? addButton.current : removes.current.get(before))
    setRows((rows) => rows.filter((r) => r !== row))
  }

  // Since and until name the error when it's theirs.
  const rangeInvalid = invalid?.range ? { 'aria-invalid': true, 'aria-describedby': 'search-error' } : {}

  return (
    <>
      <h1 tabIndex={-1}>Messages</h1>
      <form className="filters" onSubmit={submit}>
        <label>
          Session
          <input name="session" list="known-sessions" placeholder="FIX.4.4:TURBOJETHUB->…" />
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
          <input type="datetime-local" step="1" name="since" defaultValue={today()} required {...rangeInvalid} />
        </label>
        <label>
          Until (UTC; a day after since if empty)
          <input type="datetime-local" step="1" name="until" {...rangeInvalid} />
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
                Tag {at + 1}
                <input
                  name="tag"
                  inputMode="numeric"
                  placeholder="11"
                  ref={(input) => {
                    if (input) tagInputs.current.set(row, input)
                    return () => void tagInputs.current.delete(row)
                  }}
                />
              </label>
              <label>
                Value {at + 1}
                <input name="value" />
              </label>
              <button
                type="button"
                aria-label={`Remove tag condition ${at + 1}`}
                onClick={() => removeRow(row, at)}
                ref={(button) => {
                  if (button) removes.current.set(row, button)
                  return () => void removes.current.delete(row)
                }}
              >
                Remove
              </button>
            </div>
          ))}
          <button type="button" ref={addButton} onClick={addRow} disabled={rows.length >= CONDITIONS_MAX}>
            Add tag condition
          </button>
        </fieldset>
        <button type="submit" className="primary">
          Search
        </button>
      </form>
      {invalid && (
        <p className="error" role="alert" id="search-error">
          {invalid.text}
        </p>
      )}
      {search && (
        <MessageList
          key={`${search.n} ${JSON.stringify(me.clients)}`}
          fetchPage={(after) => api.messages(after === null ? search.params : [...search.params, ['after', after]])}
        />
      )}
    </>
  )
}
