import { act, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import Orders from './Orders.tsx'
import { initial, LiveContext, reduce, type LiveState } from './store.ts'
import type { LiveMessage, OrderView, OrdersPage } from './types.ts'

const FUNDA = 'FIX.4.4:JETBRIDGE->FUNDA'
const FUNDB = 'FIX.4.4:JETBRIDGE->FUNDB/DESK'

const order = (id: number, over: Partial<OrderView> = {}): OrderView => ({
  id,
  order_id: `O${id}`,
  client: FUNDA,
  cl_ord_id: `C${id}`,
  venue: 'NASDAQ',
  symbol: 'MSFT',
  side: 'buy',
  ord_type: 'limit',
  qty: '100',
  price: '10.250000000000000001',
  cum_qty: '0',
  avg_px: '0',
  status: 'new',
  pending_cancel: false,
  pending_replace: false,
  gateway_cancel: null,
  unknown_at_venue: false,
  created: `2026-10-14T09:00:0${id}Z`,
  ...over,
})

const snapshot = (orders: OrderView[], orders_next: number | null = null): LiveState =>
  reduce(initial, { type: 'snapshot', sessions: [], orders, orders_next, blocked: [], unavailable: false })

const view = (state: LiveState) => (
  <LiveContext.Provider value={{ ...state, link: 'open' }}>
    <Orders />
  </LiveContext.Provider>
)

let pages: OrdersPage[] = []
let calls: string[] = []

beforeEach(() => {
  pages = []
  calls = []
  vi.stubGlobal('fetch', async (url: string) => {
    calls.push(url)
    const page = pages.shift()
    return page ? new Response(JSON.stringify(page)) : new Response('{"error":"a range covers 31 days at most"}', { status: 400 })
  })
})

afterEach(() => {
  vi.unstubAllGlobals()
})

/** The rows' OrderIDs, top to bottom. */
const shown = () => screen.queryAllByRole('link').map((link) => link.textContent)
const row = (orderId: string) => screen.getByRole('link', { name: orderId }).closest('tr')!

/** Fills the filters given, by label, then applies them. */
async function filter(fields: Record<string, string | boolean>) {
  for (const [label, value] of Object.entries(fields)) {
    const input = screen.getByLabelText(label) as HTMLInputElement
    if (typeof value === 'boolean') {
      if (input.checked !== value) fireEvent.click(input)
    } else fireEvent.change(input, { target: { value } })
  }
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Apply' })))
}

test('live rows are the store’s working orders, newest first, and follow its events', async () => {
  const before = snapshot([order(1), order(2)])
  const { rerender } = render(view(before))
  expect(shown()).toEqual(['O2', 'O1'])
  expect(calls).toEqual([])
  expect(screen.getByRole('link', { name: 'O2' }).getAttribute('href')).toBe('#/orders/O2')

  const event = (state: LiveState, message: Partial<OrderView> & { id: number }) =>
    reduce(state, { type: 'order', ...order(message.id), ...message } as LiveMessage)
  let state = event(before, { id: 1, status: 'partially_filled', cum_qty: '40.5' })
  state = event(state, { id: 3 })
  state = event(state, { id: 2, status: 'filled', cum_qty: '100' })
  rerender(view(state))
  // A done order leaves the working view.
  expect(shown()).toEqual(['O3', 'O1'])
  expect(row('O1').textContent).toContain('40.5')

  // All of today's: the book's done ones too, read over HTTP, updated by later events.
  pages = [{ orders: [order(1), order(2, { status: 'filled', cum_qty: '100' }), order(9, { status: 'canceled', created: '2026-10-14T08:00:00Z' })], next: null }]
  await filter({ 'Working only': false })
  expect(calls).toEqual(['/api/orders'])
  // The store's newer than the page for 1 and 2.
  expect(shown()).toEqual(['O3', 'O2', 'O1', 'O9'])
  expect(row('O1').textContent).toContain('partially filled')

  state = event(state, { id: 9, status: 'canceled', unknown_at_venue: true, created: '2026-10-14T08:00:00Z' })
  state = event(state, { id: 3, status: 'rejected' })
  rerender(view(state))
  // A row done stays in this view, and a done one read over HTTP follows its events.
  expect(shown()).toEqual(['O3', 'O2', 'O1', 'O9'])
  expect(row('O3').textContent).toContain('rejected')
  expect(within(row('O9')).getByText('unknown at venue').className).toBe('badge error')
})

test('the filters build the query, and filter the store’s rows', async () => {
  render(view(snapshot([order(1), order(2, { client: FUNDB }), order(3, { symbol: 'AAPL' }), order(4, { venue: 'NYSE' })])))
  await filter({ Client: 'comp:FUNDB' })
  expect(shown()).toEqual(['O2'])
  await filter({ Client: `session:${FUNDA}`, Symbol: 'MSFT', Venue: 'NASDAQ' })
  expect(shown()).toEqual(['O1'])
  await filter({ Client: 'FUNDA' })
  expect(screen.getByRole('alert').textContent).toBe('Client is comp:COMPID or session:SESSION.')
  expect(calls).toEqual([])

  pages = [{ orders: [], next: null }]
  await filter({ Client: 'comp:FUNDA', Venue: 'NASDAQ', Symbol: 'MSFT', Status: 'filled', 'Working only': false })
  expect(calls).toEqual(['/api/orders?client=comp%3AFUNDA&venue=NASDAQ&symbol=MSFT&status=filled'])
  expect(screen.queryByRole('alert')).toBeNull()
})

test('"More" reads on from next: the snapshot’s, a page’s number, a range’s string', async () => {
  render(view(snapshot([order(1)], 1)))
  pages = [{ orders: [order(2)], next: null }]
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'More' })))
  expect(calls).toEqual(['/api/orders?working=true&after=1'])
  expect(shown()).toEqual(['O2', 'O1'])
  expect(screen.queryByRole('button', { name: 'More' })).toBeNull()

  calls = []
  pages = [{ orders: [order(1)], next: 1 }, { orders: [order(5, { status: 'filled' })], next: null }]
  await filter({ Venue: 'NASDAQ', 'Working only': false })
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'More' })))
  expect(calls).toEqual(['/api/orders?venue=NASDAQ', '/api/orders?venue=NASDAQ&after=1'])
  expect(shown()).toEqual(['O5', 'O1'])

  calls = []
  pages = [{ orders: [order(7)], next: 'b3BhcXVl' }, { orders: [order(8)], next: null }]
  await filter({ 'From (UTC)': '2026-10-01' })
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'More' })))
  expect(calls[1]).toBe(`${calls[0]}&after=b3BhcXVl`)
  expect(shown()).toEqual(['O8', 'O7'])
})

test('past mode reads the range, UTC days, to the end of the last', async () => {
  // Live updates don't reach past rows.
  const state = snapshot([order(1, { status: 'partially_filled' })])
  render(view(state))
  pages = [{ orders: [order(1)], next: null }]
  await filter({ 'From (UTC)': '2026-09-01', 'To (UTC, inclusive)': '2026-10-01' })
  expect(calls).toEqual([
    '/api/orders?working=true&from=2026-09-01T00%3A00%3A00.000Z&to=2026-10-02T00%3A00%3A00.000Z',
  ])
  expect(screen.getByRole('heading').textContent).toBe('Past orders')
  expect(row('O1').textContent).toContain('new')

  // One day: to is from.
  pages = [{ orders: [], next: null }]
  await filter({ 'Working only': false, 'To (UTC, inclusive)': '' })
  expect(calls[1]).toBe('/api/orders?from=2026-09-01T00%3A00%3A00.000Z&to=2026-09-02T00%3A00%3A00.000Z')

  // Over 31 days is refused here; the API's refusals are shown too.
  await filter({ 'To (UTC, inclusive)': '2026-10-02' })
  expect(screen.getByRole('alert').textContent).toBe('A range covers 31 days at most.')
  expect(calls).toHaveLength(2)
  await filter({ 'To (UTC, inclusive)': '2026-09-30' })
  expect(screen.getByRole('alert').textContent).toBe('a range covers 31 days at most')
})

test('flags show as badges, and decimals as the API wrote them', () => {
  render(
    view(
      snapshot([
        order(1, { pending_cancel: true, pending_replace: true, avg_px: '10.10', cum_qty: '100.000' }),
        order(2, { gateway_cancel: { state: 'stuck', tries: 3 } }),
        order(3, { gateway_cancel: { state: 'in_flight', tries: 1 }, unknown_at_venue: true, price: null, ord_type: 'market' }),
      ]),
    ),
  )
  const badges = (orderId: string) =>
    [...row(orderId).querySelectorAll('.badge')].map((badge) => `${badge.className}: ${badge.textContent}`)
  expect(badges('O1')).toEqual(['badge: cancel pending', 'badge: replace pending'])
  expect(badges('O2')).toEqual(['badge warn: gateway cancel stuck'])
  expect(badges('O3')).toEqual(['badge: gateway cancel in flight', 'badge error: unknown at venue'])
  const cells = [...row('O1').querySelectorAll('td.num')].map((cell) => cell.textContent)
  expect(cells).toEqual(['100', '10.250000000000000001', '100.000', '10.10'])
})

test('a new snapshot reads again, dropping the pages read before', async () => {
  const { rerender } = render(view(snapshot([])))
  pages = [{ orders: [order(1)], next: 1 }, { orders: [order(5, { status: 'filled' })], next: null }]
  await filter({ 'Working only': false })
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'More' })))
  expect(shown()).toEqual(['O5', 'O1'])

  // Say the scope changed: the snapshot's orders, and a page read again.
  pages = [{ orders: [order(2, { status: 'filled' })], next: null }]
  const again = reduce(snapshot([]), { type: 'snapshot', sessions: [], orders: [order(3)], orders_next: null, blocked: [], unavailable: false })
  await act(async () => rerender(view(again)))
  expect(calls).toEqual(['/api/orders', '/api/orders?after=1', '/api/orders'])
  expect(shown()).toEqual(['O3', 'O2'])
})
