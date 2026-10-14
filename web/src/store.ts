import { createContext, createElement, useContext, useEffect, useReducer, useState, type ReactNode } from 'react'
import { connect, type Opener, type Status as LinkStatus } from './live.ts'
import type { Alarm, LiveMessage, OrderView, SessionChange, SessionView, Status } from './types.ts'

/** The alarms kept, newest first. */
export const MAX_ALARMS = 100

/** What `/api/live` has said: its last snapshot with the events since applied. */
export interface LiveState {
  /** Whether a snapshot has come: until then the rest is empty, not known to be. */
  ready: boolean
  sessions: SessionView[]
  /** The working orders, by `id`. */
  orders: Map<number, OrderView>
  /**
   * The orders done since the console opened, by `id`, as their last event said: so a screen
   * showing done orders read over HTTP can show their latest. A snapshot keeps them.
   */
  finished: Map<number, OrderView>
  /** The snapshot's: with a full page, the `after` for `GET /api/orders?working=true`. */
  ordersNext: number | null
  /** Scopes as written: `all`, `comp:COMPID`, `session:SESSION`. */
  blocked: string[]
  unavailable: boolean
  /** Newest first, [`MAX_ALARMS`] at most. Not in a snapshot: those kept since the console opened. */
  alarms: Alarm[]
}

export const initial: LiveState = {
  ready: false,
  sessions: [],
  orders: new Map(),
  finished: new Map(),
  ordersNext: null,
  blocked: [],
  unavailable: false,
  alarms: [],
}

/** `book::Status::is_done`: no longer working. */
export const done = (status: Status) => status === 'filled' || status === 'canceled' || status === 'expired' || status === 'rejected'

/**
 * `message` applied to `state`, as `api::live` says a console does: a snapshot replaces all but
 * the alarms, an order event replaces its order (or adds it, or moves it to `finished` once done), and the rest
 * change what they name. Pure: `state` isn't changed.
 */
export function reduce(state: LiveState, message: LiveMessage): LiveState {
  switch (message.type) {
    case 'snapshot':
      return {
        ...state,
        ready: true,
        sessions: message.sessions,
        orders: new Map(message.orders.map((order) => [order.id, order])),
        ordersNext: message.orders_next,
        blocked: message.blocked,
        unavailable: message.unavailable,
      }
    case 'order': {
      const { type: _, ...order } = message
      // ponytail: a copy of the working orders per event, O(page) each, and of the finished ones
      // per order done, O(the day's); a burst of fills on a full page is O(page²). Batch events
      // per animation frame, or give orders their own context, if that shows.
      const orders = new Map(state.orders)
      if (!done(order.status)) return { ...state, orders: orders.set(order.id, order) }
      orders.delete(order.id)
      return { ...state, orders, finished: new Map(state.finished).set(order.id, order) }
    }
    case 'blocked':
      return state.blocked.includes(message.scope) ? state : { ...state, blocked: [...state.blocked, message.scope] }
    case 'unblocked':
      return { ...state, blocked: state.blocked.filter((scope) => scope !== message.scope) }
    case 'alarm': {
      const { type: _, ...alarm } = message
      const unavailable =
        alarm.kind === 'gateway_unavailable' ? true : alarm.kind === 'gateway_available' ? false : state.unavailable
      return { ...state, unavailable, alarms: [alarm, ...state.alarms].slice(0, MAX_ALARMS) }
    }
    case 'session': {
      const sessions = changed(state.sessions, message)
      return sessions === state.sessions ? state : { ...state, sessions }
    }
    default:
      // A type newer than this console: nothing it knows to change.
      return state
  }
}

/**
 * `sessions` with `change` applied to the one it names, added if new (a client's first
 * connection). A client disconnected and not paused is dropped, as the snapshot leaves it out:
 * venues are always listed, and paused clients so they can be resumed.
 */
function changed(sessions: SessionView[], change: SessionChange): SessionView[] {
  const at = sessions.findIndex((session) => session.side === change.side && session.id === change.id)
  const session: SessionView = sessions[at] ?? {
    side: change.side,
    id: change.id,
    venue: null,
    status: 'disconnected',
    next_incoming: null,
    next_outgoing: null,
    activity: null,
    paused: false,
  }
  const next = { ...session }
  switch (change.event) {
    case 'connected':
      next.status = 'connected'
      // A session connected since the snapshot shows resends too. `since` is when it's heard of,
      // the reducer's one impurity.
      next.activity = { since: new Date().toISOString(), last_received: null, last_sent: null, resending: false }
      break
    case 'logged_on':
    case 'logging_out':
      next.status = change.event
      break
    case 'disconnected':
      next.status = 'disconnected'
      next.activity = null
      break
    case 'paused':
    case 'resumed':
      next.paused = change.event === 'paused'
      break
    case 'resend_started':
    case 'resend_finished':
      if (next.activity) next.activity = { ...next.activity, resending: change.event === 'resend_started' }
      break
    default:
      // `refused` (its disconnection follows), or a newer event: nothing shown changes.
      return sessions
  }
  if (next.side === 'client' && next.status === 'disconnected' && !next.paused) {
    return at < 0 ? sessions : sessions.toSpliced(at, 1)
  }
  return at < 0 ? [...sessions, next] : sessions.with(at, next)
}

/** The live state, and how its socket stands. */
export type Live = LiveState & { link: LinkStatus }

export const LiveContext = createContext<Live>({ ...initial, link: 'connecting' })

/** The live state, from the nearest [`LiveProvider`]. */
export const useLive = () => useContext(LiveContext)

/**
 * Opens `/api/live` while mounted, and gives its state to those below. `open` makes the socket:
 * a fake one in tests. It must be a stable function (defined once, outside a component): a new one
 * each render closes the socket and opens another.
 */
export function LiveProvider({ children, open }: { children: ReactNode; open?: Opener }) {
  const [state, dispatch] = useReducer(reduce, initial)
  const [link, setLink] = useState<LinkStatus>('connecting')
  useEffect(() => connect({ onMessage: dispatch, onStatus: setLink, open }), [open])
  return createElement(LiveContext.Provider, { value: { ...state, link } }, children)
}
