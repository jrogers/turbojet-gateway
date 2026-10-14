import { expect, test } from 'vitest'
import { initial, MAX_ALARMS, MAX_FINISHED, reduce, type LiveState } from './store.ts'
import type { Alarm, LiveMessage, OrderView, SessionChange, SessionView, Snapshot } from './types.ts'

const order = (id: number, status: OrderView['status'] = 'new'): OrderView => ({
  id,
  order_id: `O${id}`,
  client: 'FIX.4.4:JETBRIDGE->FUNDA',
  cl_ord_id: `C${id}`,
  venue: 'NASDAQ',
  symbol: 'MSFT',
  side: 'buy',
  ord_type: 'limit',
  qty: '100',
  price: '10.25',
  cum_qty: '0',
  avg_px: '0',
  status,
  pending_cancel: false,
  pending_replace: false,
  gateway_cancel: null,
  unknown_at_venue: false,
  created: '2026-10-14T09:00:00Z',
})

const session = (side: SessionView['side'], id: string): SessionView => ({
  side,
  id,
  venue: side === 'venue' ? 'NASDAQ' : null,
  status: 'logged_on',
  next_incoming: 5,
  next_outgoing: 7,
  activity: { since: '2026-10-14T09:00:00Z', last_received: null, last_sent: null, resending: false },
  paused: false,
})

const FUNDA = 'FIX.4.4:JETBRIDGE->FUNDA'
const VENUE = 'FIX.4.4:JETBRIDGE->NASDAQ'

const snapshot = (over: Partial<Snapshot> = {}): Snapshot => ({
  type: 'snapshot',
  sessions: [session('client', FUNDA), session('venue', VENUE)],
  orders: [order(1), order(2)],
  orders_next: null,
  blocked: [],
  unavailable: false,
  clients: null,
  ...over,
})

const alarm = (kind: Alarm['kind'], text = ''): LiveMessage => ({
  type: 'alarm',
  time: '2026-10-14T09:00:00Z',
  kind,
  client: null,
  text,
})

const apply = (state: LiveState, ...messages: LiveMessage[]) => messages.reduce(reduce, state)

test('a snapshot, then events, give the state they say', () => {
  const before = apply(initial, snapshot())
  const state = apply(
    before,
    { type: 'order', ...order(2, 'partially_filled') },
    { type: 'order', ...order(3) },
    { type: 'order', ...order(1, 'canceled') },
    { type: 'blocked', scope: 'comp:FUNDA' },
    { type: 'blocked', scope: 'all' },
    { type: 'blocked', scope: 'all' },
    { type: 'unblocked', scope: 'comp:FUNDA' },
    alarm('gateway_unavailable'),
    { type: 'session', side: 'client', id: FUNDA, event: 'paused' },
    { type: 'session', side: 'client', id: FUNDA, event: 'disconnected' },
    { type: 'session', side: 'venue', id: VENUE, event: 'resend_started' },
    // The same id on the other side is another session.
    { type: 'session', side: 'client', id: VENUE, event: 'connected' },
  )
  expect(state.ready).toBe(true)
  expect([...state.orders.keys()]).toEqual([2, 3])
  expect(state.orders.get(2)?.status).toBe('partially_filled')
  expect(state.orders.get(2)).not.toHaveProperty('type')
  expect([...state.finished.keys()]).toEqual([1])
  expect(state.finished.get(1)?.status).toBe('canceled')
  expect(state.blocked).toEqual(['all'])
  expect(state.unavailable).toBe(true)
  expect(state.alarms.map((a) => a.kind)).toEqual(['gateway_unavailable'])
  expect(state.alarms[0]).not.toHaveProperty('type')
  const [funda, venue, added] = state.sessions
  expect(funda).toMatchObject({ status: 'disconnected', paused: true, activity: null })
  expect(venue).toMatchObject({ status: 'logged_on', activity: { resending: true } })
  expect(added).toMatchObject({ side: 'client', id: VENUE, status: 'connected', next_incoming: null })
  // Pure: what came before is as it was.
  expect([...before.orders.keys()]).toEqual([1, 2])
  expect(before.sessions[0].paused).toBe(false)
  expect(before.blocked).toEqual([])
})

test("a lag's snapshot replaces all but the alarms", () => {
  const state = apply(
    initial,
    snapshot(),
    { type: 'order', ...order(3) },
    { type: 'blocked', scope: 'all' },
    alarm('orphan', 'kept'),
    snapshot({ sessions: [], orders: [order(9)], orders_next: 9, blocked: ['comp:FUNDB'], unavailable: true }),
  )
  expect(state.sessions).toEqual([])
  expect([...state.orders.keys()]).toEqual([9])
  expect(state.ordersNext).toBe(9)
  expect(state.blocked).toEqual(['comp:FUNDB'])
  expect(state.unavailable).toBe(true)
  expect(state.alarms.map((a) => a.text)).toEqual(['kept'])
  expect(state.snapshots).toBe(2)
  // A snapshot empties the finished: they may be out of its scope.
  const done = apply(state, { type: 'order', ...order(9, 'filled') })
  expect([...done.finished.keys()]).toEqual([9])
  expect(apply(done, snapshot()).finished.size).toBe(0)
})

test("a snapshot under other clients keeps only the gateway-wide alarms; the first keeps them all", () => {
  const kinds = (state: LiveState) => state.alarms.map((a) => a.kind)
  // Alarms can come before the first snapshot, from a socket's first events.
  const first = apply(initial, alarm('stuck_cancel'), snapshot({ clients: ['FUNDA'] }))
  expect(kinds(first)).toEqual(['stuck_cancel'])
  expect(first.clients).toEqual(['FUNDA'])
  const held = apply(first, alarm('gateway_unavailable'), alarm('overfill'), alarm('venue_store_reset'), alarm('gateway_available'))
  const same = apply(held, snapshot({ clients: ['FUNDA'] }))
  expect(kinds(same)).toEqual(kinds(held))
  const rescoped = apply(held, snapshot({ clients: ['FUNDB'] }))
  expect(kinds(rescoped)).toEqual(['gateway_available', 'venue_store_reset', 'gateway_unavailable'])
  expect(rescoped.clients).toEqual(['FUNDB'])
  // Unscoped is a change too.
  expect(kinds(apply(held, snapshot({ clients: null })))).toEqual(kinds(rescoped))
})

test('alarms are kept newest first, so many at most; the gateway coming back is available', () => {
  const alarms = Array.from({ length: MAX_ALARMS + 5 }, (_, n) => alarm('orphan', `${n}`))
  const state = apply(initial, snapshot({ unavailable: true }), ...alarms, alarm('gateway_available', 'back'))
  expect(state.alarms).toHaveLength(MAX_ALARMS)
  expect(state.alarms[0].text).toBe('back')
  expect(state.alarms[1].text).toBe(`${MAX_ALARMS + 4}`)
  expect(state.unavailable).toBe(false)
})

test("an event it doesn't know changes nothing", () => {
  const state = apply(initial, snapshot())
  expect(reduce(state, { type: 'session', side: 'client', id: FUNDA, event: 'refused', reason: 'bad password' })).toBe(state)
  expect(reduce(state, { type: 'something_new' } as unknown as LiveMessage)).toBe(state)
})

test('a session connected since the snapshot shows its resends', () => {
  const state = apply(
    initial,
    snapshot(),
    { type: 'session', side: 'venue', id: VENUE, event: 'disconnected' },
    { type: 'session', side: 'venue', id: VENUE, event: 'connected' },
    { type: 'session', side: 'venue', id: VENUE, event: 'resend_started' },
  )
  expect(state.sessions[1]).toMatchObject({ status: 'connected', activity: { resending: true, last_received: null } })
})

test('a client disconnected and not paused is dropped, as a snapshot leaves it out; a venue stays', () => {
  const event = (side: SessionView['side'], id: string, event: SessionChange['event']): LiveMessage => ({
    type: 'session',
    side,
    id,
    event,
  })
  const ids = (state: LiveState) => state.sessions.map((s) => `${s.side} ${s.id}`)
  const start = apply(initial, snapshot())
  const down = apply(start, event('client', FUNDA, 'disconnected'), event('venue', VENUE, 'disconnected'))
  expect(ids(down)).toEqual([`venue ${VENUE}`])

  // Paused, it stays, to be resumed; resumed while disconnected, it goes.
  const paused = apply(start, event('client', FUNDA, 'paused'), event('client', FUNDA, 'disconnected'))
  expect(ids(paused)).toEqual([`client ${FUNDA}`, `venue ${VENUE}`])
  expect(ids(apply(paused, event('client', FUNDA, 'resumed')))).toEqual([`venue ${VENUE}`])
  // And comes back when it connects.
  expect(ids(apply(paused, event('client', FUNDA, 'resumed'), event('client', FUNDA, 'connected')))).toEqual([
    `venue ${VENUE}`,
    `client ${FUNDA}`,
  ])
})

test('the finished orders kept are the latest, so many at most', () => {
  const fills = Array.from({ length: MAX_FINISHED + 2 }, (_, n): LiveMessage => ({ type: 'order', ...order(n + 1, 'filled') }))
  const state = apply(initial, snapshot(), ...fills)
  expect(state.finished.size).toBe(MAX_FINISHED)
  expect([...state.finished.keys()].slice(0, 2)).toEqual([3, 4])
  expect(state.finished.has(MAX_FINISHED + 2)).toBe(true)
})
