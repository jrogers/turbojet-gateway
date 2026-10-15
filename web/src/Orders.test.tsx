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
  reduce(initial, { type: 'snapshot', sessions: [], orders, orders_next, blocked: [], unavailable: false, clients: null })

const view = (state: LiveState) => (
  <LiveContext.Provider value={{ ...state, link: 'open' }}>
    <Orders />
  </LiveContext.Provider>
)

let pages: (OrdersPage | Response)[] = []
let calls: string[] = []

beforeEach(() => {
  pages = []
  calls = []
  vi.stubGlobal('fetch', async (url: string) => {
    calls.push(url)
    const page = pages.shift() ?? new Response('{"error":"no page queued"}', { status: 500 })
    return page instanceof Response ? page : new Response(JSON.stringify(page))
  })
})

afterEach(() => {
  vi.unstubAllGlobals()
})

/** The rows' OrderIDs, top to bottom. */
const shown = () => screen.queryAllByRole('link').map((link) => link.textContent)
const row = (orderId: string) => screen.getByRole('link', { name: orderId }).closest('tr')!
const more = () => act(async () => fireEvent.click(screen.getByRole('button', { name: 'More' })))

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

const event = (state: LiveState, change: Partial<OrderView> & { id: number }) =>
  reduce(state, { type: 'order', ...order(change.id), ...change } as LiveMessage)

test('live rows are the store’s working orders, newest first, and follow its events', async () => {
  const before = snapshot([order(2), order(3)])
  const { rerender } = render(view(before))
  expect(shown()).toEqual(['O3', 'O2'])
  expect(calls).toEqual([])
  expect(screen.getByRole('link', { name: 'O2' }).getAttribute('href')).toBe('#/orders/O2')

  let state = event(before, { id: 2, status: 'partially_filled', cum_qty: '40.5' })
  state = event(state, { id: 4 })
  state = event(state, { id: 3, status: 'filled', cum_qty: '100' })
  rerender(view(state))
  // A done order leaves the working view.
  expect(shown()).toEqual(['O4', 'O2'])
  expect(row('O2').textContent).toContain('40.5')

  // All of today's, newest first: the book's done ones too, read over HTTP, a page so far.
  pages = [{ orders: [order(4), order(3, { status: 'filled', cum_qty: '100' }), order(2)], next: 2 }]
  await filter({ 'Working only': false })
  expect(calls).toEqual(['/api/orders?desc=true'])
  // The store's newer than the page for 2.
  expect(shown()).toEqual(['O4', 'O3', 'O2'])
  expect(row('O2').textContent).toContain('partially filled')

  state = event(state, { id: 3, status: 'filled', unknown_at_venue: true })
  state = event(state, { id: 4, status: 'rejected' })
  state = event(state, { id: 5 })
  // Below the newest read and not on the page: on one not read yet.
  state = event(state, { id: 1, status: 'canceled' })
  rerender(view(state))
  // A row done stays in this view, and a done one read over HTTP follows its events.
  expect(shown()).toEqual(['O5', 'O4', 'O3', 'O2'])
  expect(row('O4').textContent).toContain('rejected')
  expect(within(row('O3')).getByText('unknown at venue').className).toBe('badge error')

  pages = [{ orders: [order(1)], next: null }]
  await more()
  expect(calls[1]).toBe('/api/orders?desc=true&after=2')
  expect(shown()).toEqual(['O5', 'O4', 'O3', 'O2', 'O1'])
  expect(row('O1').textContent).toContain('canceled')
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
  expect(calls).toEqual(['/api/orders?client=comp%3AFUNDA&venue=NASDAQ&symbol=MSFT&status=filled&desc=true'])
  expect(screen.queryByRole('alert')).toBeNull()
})

test('"More" reads on from next: the snapshot’s, a page’s number, a range’s string', async () => {
  render(view(snapshot([order(1)], 1)))
  // Working only, on up from the snapshot's.
  pages = [{ orders: [order(2)], next: null }]
  await more()
  expect(calls).toEqual(['/api/orders?working=true&after=1'])
  expect(shown()).toEqual(['O2', 'O1'])
  expect(screen.queryByRole('button', { name: 'More' })).toBeNull()

  calls = []
  pages = [{ orders: [order(5, { status: 'filled' })], next: 5 }, { orders: [order(1)], next: null }]
  await filter({ Venue: 'NASDAQ', 'Working only': false })
  await more()
  expect(calls).toEqual(['/api/orders?venue=NASDAQ&desc=true', '/api/orders?venue=NASDAQ&desc=true&after=5'])
  expect(shown()).toEqual(['O5', 'O1'])

  calls = []
  pages = [{ orders: [order(8)], next: 'b3BhcXVl' }, { orders: [order(7)], next: null }]
  await filter({ 'From (UTC)': '2026-10-01' })
  await more()
  expect(calls[1]).toBe(`${calls[0]}&after=b3BhcXVl`)
  expect(shown()).toEqual(['O8', 'O7'])
})

test('past mode reads the range, UTC days, to the end of the last, in the server’s order', async () => {
  // Live updates don't reach past rows.
  render(view(snapshot([order(1, { status: 'partially_filled' })])))
  pages = [{ orders: [order(1), order(2)], next: null }]
  await filter({ 'From (UTC)': '2026-09-01', 'To (UTC, inclusive)': '2026-10-01' })
  // Working only is live only: ignored, and disabled.
  expect(calls).toEqual(['/api/orders?desc=true&from=2026-09-01T00%3A00%3A00.000Z&to=2026-10-02T00%3A00%3A00.000Z'])
  expect((screen.getByLabelText('Working only') as HTMLInputElement).disabled).toBe(true)
  expect(screen.getByRole('heading').textContent).toBe('Past orders')
  expect(shown()).toEqual(['O1', 'O2'])
  expect(row('O1').textContent).toContain('new')

  // One day: to is from.
  pages = [{ orders: [], next: null }]
  await filter({ 'To (UTC, inclusive)': '' })
  expect(calls[1]).toBe('/api/orders?desc=true&from=2026-09-01T00%3A00%3A00.000Z&to=2026-09-02T00%3A00%3A00.000Z')

  // Ranges refused here, then the API's refusals shown too.
  for (const [to, error] of [
    ['2026-10-02', 'A range covers 31 days at most.'],
    ['2026-08-31', 'To is before from.'],
  ]) {
    await filter({ 'To (UTC, inclusive)': to })
    expect(screen.getByRole('alert').textContent).toBe(error)
  }
  await filter({ 'From (UTC)': '' })
  expect(screen.getByRole('alert').textContent).toBe('To needs a from date.')
  expect((screen.getByLabelText('Working only') as HTMLInputElement).disabled).toBe(false)
  expect(calls).toHaveLength(2)
  pages = [new Response('{"error":"no [database]: only the current day is kept"}', { status: 400 })]
  await filter({ 'From (UTC)': '2026-09-01', 'To (UTC, inclusive)': '2026-09-30' })
  expect(screen.getByRole('alert').textContent).toBe('no [database]: only the current day is kept')
})

test('a read for an earlier query is dropped, whenever it comes', async () => {
  const answers: ((page: OrdersPage) => void)[] = []
  vi.stubGlobal('fetch', (url: string) => {
    calls.push(url)
    return new Promise<Response>((resolve) => answers.push((page) => resolve(new Response(JSON.stringify(page)))))
  })
  render(view(snapshot([])))
  await filter({ 'Working only': false, Symbol: 'AAPL' })
  await filter({ Symbol: 'MSFT' })
  expect(calls).toEqual(['/api/orders?symbol=AAPL&desc=true', '/api/orders?symbol=MSFT&desc=true'])
  await act(async () => answers[1]({ orders: [order(2)], next: null }))
  await act(async () => answers[0]({ orders: [order(1, { symbol: 'AAPL' })], next: 1 }))
  expect(shown()).toEqual(['O2'])
  expect(screen.queryByRole('button', { name: 'More' })).toBeNull()
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
  pages = [{ orders: [order(5, { status: 'filled' })], next: 5 }, { orders: [order(1)], next: null }]
  await filter({ 'Working only': false })
  await more()
  expect(shown()).toEqual(['O5', 'O1'])

  // Say the scope changed: the snapshot's orders, and a page read again.
  pages = [{ orders: [order(2, { status: 'filled' })], next: null }]
  const again = reduce(snapshot([]), { type: 'snapshot', sessions: [], orders: [order(3)], orders_next: null, blocked: [], unavailable: false, clients: null })
  await act(async () => rerender(view(again)))
  expect(calls).toEqual(['/api/orders?desc=true', '/api/orders?desc=true&after=5', '/api/orders?desc=true'])
  expect(shown()).toEqual(['O3', 'O2'])
})

test('a row the feed changes flashes, its status a pill and its fill a bar', () => {
  const flashing = (orderId: string) => row(orderId).classList.contains('flash')
  const before = snapshot([order(1), order(2)])
  const { rerender } = render(view(before))
  expect(flashing('O1')).toBe(false)
  expect(row('O1').querySelector('.pill')!.className).toBe('pill info')
  expect(row('O1').querySelector('td.buy')!.textContent).toBe('buy')

  let state = event(before, { id: 1, status: 'partially_filled', cum_qty: '40', side: 'sell_short' })
  rerender(view(state))
  expect(flashing('O1')).toBe(true)
  expect(flashing('O2')).toBe(false)
  expect(within(row('O1')).getByText('partially filled').className).toBe('pill warn')
  expect(row('O1').querySelector('td.sell')!.textContent).toBe('sell short')
  const bar = row('O1').querySelector<HTMLElement>('.bar')!
  expect(bar.getAttribute('aria-hidden')).toBe('true')
  expect((bar.firstChild as HTMLElement).style.width).toBe('40%')

  // Another event before it ends restarts it, and one end takes it off.
  state = event(state, { id: 1, status: 'partially_filled', cum_qty: '50', side: 'sell_short' })
  rerender(view(state))
  expect(flashing('O1')).toBe(true)
  fireEvent.animationEnd(row('O1'))
  expect(flashing('O1')).toBe(false)

  // A snapshot's objects are new, but unchanged ones don't flash.
  rerender(view(snapshot([order(1, { status: 'partially_filled', cum_qty: '50', side: 'sell_short' }), order(2)])))
  expect(flashing('O1')).toBe(false)
})
