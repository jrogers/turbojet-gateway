import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import * as api from './api.ts'
import { StatusText } from './Orders.tsx'
import MessageList from './MessageList.tsx'
import Reason from './Reason.tsx'
import { done, useLive } from './store.ts'
import { utc } from './time.ts'
import type { Me, OrderDetail } from './types.ts'

type Action = 'cancel' | 'status' | 'resolve'

const ACTIONS: Record<Action, { title: string; post: (orderId: string, reason: string) => Promise<void>; done: string }> = {
  cancel: { title: 'Cancel order', post: api.cancelOrder, done: 'Cancel sent to the venue.' },
  status: { title: 'Ask the venue for status', post: api.requestStatus, done: 'Status asked of the venue; its answer comes as the order’s reports.' },
  resolve: { title: 'Resolve order', post: api.resolve, done: 'Resolved: the order is canceled.' },
}


const message = (e: unknown) => (e instanceof Error ? e.message : String(e))

/**
 * An order's page: its fields, kept live by the store's events for it, its requests to the venue
 * as last read, and its messages on both sides from the message log. An operator or admin may
 * cancel it, ask its venue for status, or resolve it once its venue has said it doesn't know it.
 * Mounted per order (keyed by the route), so nothing carries over from another.
 */
export default function Order({ orderId, me }: { orderId: string; me: Me }) {
  const live = useLive()
  // `undefined` until read; `null` for a 404 or a 403 (out of the user's clients, perhaps since a
  // reload), with `missing` saying why.
  const [detail, setDetail] = useState<OrderDetail | null>()
  const [missing, setMissing] = useState('No such order.')
  const heading = useRef<HTMLHeadingElement>(null)
  // Set by an action done: focus goes to the heading once its dialog has closed, as the button
  // that opened it may be gone.
  const refocus = useRef(false)
  const [error, setError] = useState<string>()
  // One more reads the order again: after an action, which changes it.
  const [reads, setReads] = useState(0)
  const [action, setAction] = useState<Action>()
  const [notice, setNotice] = useState<string>()

  // The store's version: the latest event for it, working or done.
  const latest = useMemo(() => {
    for (const order of [...live.orders.values(), ...live.finished.values()]) {
      if (order.order_id === orderId) return order
    }
  }, [live.orders, live.finished, orderId])

  // Read again on each snapshot: events may have been missed. A read answering after a newer one
  // was asked is dropped.
  const asking = useRef<object>(undefined)
  useEffect(() => {
    const at = {}
    asking.current = at
    api.order(orderId).then(
      (order) => {
        if (asking.current !== at) return
        setDetail(order)
        setError(undefined)
      },
      (e: unknown) => {
        if (asking.current !== at) return
        if (e instanceof api.ApiError && (e.status === 404 || e.status === 403)) {
          setDetail(null)
          setMissing(e.status === 403 ? e.message : 'No such order.')
        } else setError(message(e))
      },
    )
    return () => {
      asking.current = undefined
    }
  }, [orderId, live.snapshots, reads])

  // After the dialog's own cleanup, which focuses where it was opened from.
  useEffect(() => {
    if (action === undefined && refocus.current) {
      refocus.current = false
      heading.current?.focus()
    }
  }, [action])

  async function confirm(reason: string) {
    if (!action) return
    await ACTIONS[action].post(orderId, reason)
    setNotice(ACTIONS[action].done)
    refocus.current = true
    setReads((reads) => reads + 1)
  }

  if (detail === null) {
    return (
      <>
        <h1 tabIndex={-1}>Order {orderId}</h1>
        <p className="error" role="alert">
          {missing}
        </p>
        <p>
          <a href="#/orders">Back to the blotter</a>
        </p>
      </>
    )
  }

  // The store's latest event over the last read: an event changes the fields, and the requests,
  // which events don't carry, stay as read.
  const shown: OrderDetail | undefined = detail && latest ? { ...detail, ...latest } : detail
  const acts = me.role !== 'viewer' && shown !== undefined && !done(shown.status)
  const choose = (kind: Action) => {
    setNotice(undefined)
    setAction(kind)
  }

  return (
    <>
      <p>
        <a href="#/orders">Orders</a>
      </p>
      <h1 ref={heading} tabIndex={-1}>
        Order {orderId}
      </h1>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      {/* Always there, so a screen reader hears its text change. */}
      <p role="status">{notice}</p>
      {shown === undefined ? (
        !error && <p className="muted">Loading…</p>
      ) : (
        <>
          {acts && (
            <p className="actions">
              <button type="button" onClick={() => choose('cancel')}>
                Cancel
              </button>
              <button type="button" onClick={() => choose('status')}>
                Ask status
              </button>
              {shown.unknown_at_venue && (
                <button type="button" onClick={() => choose('resolve')}>
                  Resolve
                </button>
              )}
            </p>
          )}
          <Fields order={shown} />
          <h2>Requests to the venue</h2>
          <Requests order={shown} />
        </>
      )}
      <h2>Messages</h2>
      <MessageList fetchPage={(after) => api.orderMessages(orderId, { after })} />
      {action && (
        <Reason title={`${ACTIONS[action].title} ${orderId}`} onConfirm={confirm} onClose={() => setAction(undefined)} />
      )}
    </>
  )
}

/** The order's fields. Amounts as the API wrote them: decimals, never floats. */
function Fields({ order }: { order: OrderDetail }) {
  const fields: [string, ReactNode][] = [
    ['Status', <StatusText order={order} />],
    ['Client', order.client],
    ['ClOrdID', order.cl_ord_id],
    ['Venue', order.venue],
    ['Venue’s OrderID', order.venue_order_id ?? ''],
    ['Symbol', order.symbol],
    ['Side', order.side.replace('_', ' ')],
    ['Type', order.ord_type],
    ['Qty', order.qty],
    ['Price', order.price ?? ''],
    ['Filled', order.cum_qty],
    ['Avg px', order.avg_px],
    ['Created (UTC)', utc(order.created)],
  ]
  return (
    <dl className="fields">
      {fields.map(([name, value]) => (
        <div key={name}>
          <dt>{name}</dt>
          <dd>{value}</dd>
        </div>
      ))}
    </dl>
  )
}

function Requests({ order }: { order: OrderDetail }) {
  if (order.requests.length === 0) return <p className="muted">None.</p>
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Downstream ClOrdID</th>
            <th>Kind</th>
            <th>Origin</th>
            <th>Client’s ClOrdID</th>
            <th>Sent (UTC)</th>
            <th>Answered</th>
          </tr>
        </thead>
        <tbody>
          {order.requests.map((request) => (
            <tr key={request.downstream}>
              <td>{request.downstream}</td>
              <td>{request.kind}</td>
              <td>{request.origin}</td>
              <td>{request.cl_ord_id ?? ''}</td>
              <td>{utc(request.sent_at)}</td>
              <td>{request.answered ? 'yes' : 'no'}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
