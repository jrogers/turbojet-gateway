import { memo, useEffect, useMemo, useRef, useState, type FormEvent } from 'react'
import * as api from './api.ts'
import { orderHref } from './router.ts'
import { done, useLive } from './store.ts'
import type { OrderView, Status } from './types.ts'

const STATUSES: Status[] = [
  'pending_new',
  'new',
  'partially_filled',
  'filled',
  'pending_cancel',
  'pending_replace',
  'canceled',
  'expired',
  'rejected',
]

/** The longest range of past orders the API reads: `api::read::HISTORY_RANGE_MAX`. */
export const MAX_DAYS = 31

const DAY = 86_400_000

/**
 * What the blotter shows: the API's filters, and with `from` (a `YYYY-MM-DD` day, UTC) past
 * orders up to the end of day `to` (`from` if empty). `working` is live only.
 */
interface Query {
  client: string
  venue: string
  symbol: string
  status: string
  working: boolean
  from: string
  to: string
}

const LIVE: Query = { client: '', venue: '', symbol: '', status: '', working: true, from: '', to: '' }

/** A page read over HTTP: its orders, and its `next` to read on from. */
interface Loaded {
  orders: OrderView[]
  next: number | string | null
}

/**
 * `query`'s parameters for `GET /api/orders`, or why it can't be asked: a client that isn't a
 * scope, or a range the API refuses. Newest first, but live and working only: those page on up
 * from the snapshot's, which is lowest first.
 */
function params(query: Query): Record<string, string | boolean | undefined> | string {
  if (query.client !== '' && !/^(all|comp:.+|session:.+)$/.test(query.client)) {
    return 'Client is comp:COMPID or session:SESSION.'
  }
  const { client, venue, symbol, status } = query
  const filters = { client, venue, symbol, status }
  if (query.from === '') {
    if (query.to !== '') return 'To needs a from date.'
    return query.working ? { ...filters, working: true } : { ...filters, desc: true }
  }
  const from = Date.parse(`${query.from}T00:00:00Z`)
  const to = Date.parse(`${query.to || query.from}T00:00:00Z`) + DAY
  if (Number.isNaN(from) || Number.isNaN(to)) return 'The dates are YYYY-MM-DD.'
  if (to <= from) return 'To is before from.'
  if (to - from > MAX_DAYS * DAY) return `A range covers ${MAX_DAYS} days at most.`
  return { ...filters, desc: true, from: new Date(from).toISOString(), to: new Date(to).toISOString() }
}

/**
 * `api::read::names_target`: whether `session` (`BEGIN:SENDER->TARGET`, maybe `/SUB`, `:QUAL`)
 * has `compId` as its TargetCompID, the client's.
 */
function namesTarget(session: string, compId: string): boolean {
  const at = session.indexOf('->')
  if (at < 0) return false
  const target = session.slice(at + 2)
  if (!target.startsWith(compId)) return false
  const rest = target.slice(compId.length)
  return rest === '' || rest[0] === '/' || rest[0] === ':'
}

/** `book::OrderFilter::matches`, for the store's orders, which no request filtered. */
function matches(order: OrderView, query: Query): boolean {
  const { client } = query
  return (
    (client === '' ||
      client === 'all' ||
      (client.startsWith('session:') && order.client === client.slice('session:'.length)) ||
      (client.startsWith('comp:') && namesTarget(order.client, client.slice('comp:'.length)))) &&
    (query.venue === '' || order.venue === query.venue) &&
    (query.symbol === '' || order.symbol === query.symbol) &&
    (query.status === '' || order.status === query.status) &&
    !(query.working && done(order.status))
  )
}

/** Newest first: by the book's `id`, in booking order, as `desc` pages them. */
const newestFirst = (a: OrderView, b: OrderView) => b.id - a.id

/**
 * The blotter. Live by default: the store's working orders, and with "working only" off today's
 * done ones too, from `GET /api/orders`, kept up to date by the live events. With a date, the
 * database's orders of those days instead, as they were read. Newest first; "More" reads the
 * next page.
 */
export default function Orders() {
  const live = useLive()
  const [query, setQuery] = useState<Query>(LIVE)
  // `undefined`: nothing read yet. Live and working only, the store's are the first page.
  const [loaded, setLoaded] = useState<Loaded>()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  // The From field as typed: past orders ignore "working only", so it's disabled with a date.
  const [from, setFrom] = useState('')
  const past = query.from !== ''
  const asked = params(query)

  // A token for the current query and snapshot, new with each: a read for earlier ones is dropped.
  const asking = useRef<object>(undefined)

  // `asked` is `query`'s, so `query` is the dependency, with the snapshots: a new one may follow
  // missed events or a new scope, so what was read is read again.
  useEffect(() => {
    const at = {}
    asking.current = at
    setLoaded(undefined)
    setError(typeof asked === 'string' ? asked : undefined)
    const reads = typeof asked !== 'string' && (past || !query.working)
    setBusy(reads)
    if (!reads) return
    api.orders(asked).then(
      (page) => asking.current === at && setLoaded(page),
      (e: unknown) => asking.current === at && setError(e instanceof Error ? e.message : String(e)),
    ).finally(() => asking.current === at && setBusy(false))
  }, [query, live.snapshots])

  const next = loaded ? loaded.next : !past && query.working && live.ready ? live.ordersNext : null

  async function more() {
    if (typeof asked === 'string' || next === null) return
    const at = asking.current
    setBusy(true)
    setError(undefined)
    try {
      const page = await api.orders({ ...asked, after: String(next) })
      if (asking.current !== at) return
      setLoaded((before) => ({ orders: [...(before?.orders ?? []), ...page.orders], next: page.next }))
    } catch (e) {
      if (asking.current === at) setError(e instanceof Error ? e.message : String(e))
    } finally {
      if (asking.current === at) setBusy(false)
    }
  }

  // ponytail: every row merged, filtered and sorted again per live event, O(rows log rows); fine
  // for a few thousand. Keep the merged map in state, updated per event, if that shows.
  const rows = useMemo(() => {
    // Past ones in the server's order, newest first.
    if (past) return loaded?.orders ?? []
    const pages = loaded?.orders ?? []
    const shown = new Map(pages.map((order) => [order.id, order]))
    // The pages read newest first reach down only so far: a live order below the newest read and
    // not on them is in a page not read yet. Working only, the store's are the first page, and
    // those read go on up from them.
    const above = query.working ? -Infinity : pages.reduce((newest, order) => Math.max(newest, order.id), -Infinity)
    // The live events are newer than any page read: a working order's in `orders`, a done one's in
    // `finished`.
    for (const order of [...live.finished.values(), ...live.orders.values()]) {
      if (order.id > above || shown.has(order.id)) shown.set(order.id, order)
    }
    return [...shown.values()].filter((order) => matches(order, query)).sort(newestFirst)
  }, [past, loaded, live.orders, live.finished, query])

  function apply(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = new FormData(event.currentTarget)
    const text = (name: string) => String(form.get(name) ?? '').trim()
    setQuery({
      client: text('client'),
      venue: text('venue'),
      symbol: text('symbol'),
      status: text('status'),
      working: form.get('working') !== null,
      from: text('from'),
      to: text('to'),
    })
  }

  return (
    <>
      <h1>{past ? 'Past orders' : 'Orders'}</h1>
      <form className="filters" onSubmit={apply}>
        <label>
          Client
          <input name="client" placeholder="comp:COMPID or session:…" />
        </label>
        <label>
          Venue
          <input name="venue" />
        </label>
        <label>
          Symbol
          <input name="symbol" />
        </label>
        <label>
          Status
          <select name="status" defaultValue="">
            <option value="">any</option>
            {STATUSES.map((status) => (
              <option key={status} value={status}>
                {status.replaceAll('_', ' ')}
              </option>
            ))}
          </select>
        </label>
        <label className="check" title={from === '' ? undefined : 'Live only: past orders are as last written.'}>
          <input type="checkbox" name="working" defaultChecked disabled={from !== ''} />
          Working only
        </label>
        <label>
          From (UTC)
          <input type="date" name="from" value={from} onChange={(e) => setFrom(e.target.value)} />
        </label>
        <label>
          To (UTC, inclusive)
          <input type="date" name="to" />
        </label>
        <button type="submit" className="primary">
          Apply
        </button>
      </form>
      <p className="muted">
        {past
          ? `Created from ${query.from} to the end of ${query.to || query.from}, UTC midnights, as last written: not live, working or not.`
          : 'Live: today’s orders. Pick dates for past ones.'}
      </p>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      {!past && query.working && !live.ready ? (
        <p className="muted">Loading…</p>
      ) : (
        <>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Created (UTC)</th>
                  <th>OrderID</th>
                  <th>Client</th>
                  <th>ClOrdID</th>
                  <th>Venue</th>
                  <th>Symbol</th>
                  <th>Side</th>
                  <th>Type</th>
                  <th className="num">Qty</th>
                  <th className="num">Price</th>
                  <th className="num">Filled</th>
                  <th className="num">Avg px</th>
                  <th>Status</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((order) => (
                  <Row key={order.id} order={order} />
                ))}
              </tbody>
            </table>
          </div>
          <p className="muted">
            {rows.length === 0 ? 'No orders.' : `${rows.length} ${rows.length === 1 ? 'order' : 'orders'}.`}{' '}
            {next !== null && (
              <button type="button" onClick={more} disabled={busy}>
                More
              </button>
            )}
          </p>
        </>
      )}
    </>
  )
}

/**
 * An order's row. Amounts as the API wrote them: decimals, never floats. Memoised: an event
 * changes one order, and the rest keep their objects.
 */
const Row = memo(function Row({ order }: { order: OrderView }) {
  return (
    <tr>
      <td>{order.created.slice(0, 19).replace('T', ' ')}</td>
      <td>
        <a href={orderHref(order.order_id)}>{order.order_id}</a>
      </td>
      <td>{order.client}</td>
      <td>{order.cl_ord_id}</td>
      <td>{order.venue}</td>
      <td>{order.symbol}</td>
      <td>{order.side.replace('_', ' ')}</td>
      <td>{order.ord_type}</td>
      <td className="num">{order.qty}</td>
      <td className="num">{order.price ?? ''}</td>
      <td className="num">{order.cum_qty}</td>
      <td className="num">{order.avg_px}</td>
      <td>
        <StatusText order={order} />
      </td>
    </tr>
  )
})

/** An order's status, and its flags as badges after it: the blotter's and the order page's. */
export function StatusText({ order }: { order: OrderView }) {
  return (
    <>
      {order.status.replaceAll('_', ' ')}
      {order.pending_cancel && <span className="badge">cancel pending</span>}
      {order.pending_replace && <span className="badge">replace pending</span>}
      {order.gateway_cancel && (
        <span className={order.gateway_cancel.state === 'stuck' ? 'badge warn' : 'badge'}>
          gateway cancel {order.gateway_cancel.state.replace('_', ' ')}
        </span>
      )}
      {order.unknown_at_venue && <span className="badge error">unknown at venue</span>}
    </>
  )
}
