import { StrictMode } from 'react'
import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import * as api from './api.ts'
import Order from './Order.tsx'
import { initial, LiveContext, reduce, type LiveState } from './store.ts'
import type { Hit, LiveMessage, Me, OrderDetail, OrderView, Page } from './types.ts'

const FUNDA = 'FIX.4.4:TURBOJETGATEWAY->FUNDA'

const view = (over: Partial<OrderView> = {}): OrderView => ({
  id: 7,
  order_id: 'O/7',
  client: FUNDA,
  cl_ord_id: 'C7',
  venue: 'NASDAQ',
  symbol: 'MSFT',
  side: 'sell_short',
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
  created: '2026-10-14T09:00:07.123Z',
  ...over,
})

const detail = (over: Partial<OrderDetail> = {}): OrderDetail => ({
  ...view(),
  venue_order_id: 'V1',
  requests: [
    { downstream: 'JB1', kind: 'new', origin: 'client', cl_ord_id: 'C7', sent_at: '2026-10-14T09:00:07.200Z', answered: true },
  ],
  ...over,
})

const hit = (raw: string, over: Partial<Hit> = {}): Hit => ({
  time: '2026-10-14T09:00:07.123456Z',
  direction: 'in',
  session: FUNDA,
  msg_type: 'D',
  fields: [
    [35, 'D'],
    [11, 'C7'],
  ],
  raw,
  ...over,
})

const PATH = '/api/orders/O%2F7'

const snapshot = (orders: OrderView[] = []): LiveState =>
  reduce(initial, { type: 'snapshot', sessions: [], orders, orders_next: null, blocked: [], unavailable: false, clients: null })

const who = (role: Me['role']): Me => ({ user: 'u', role, clients: null })

const page = (orderId: string, me: Me, state: LiveState) => (
  <LiveContext.Provider value={{ ...state, link: 'open' }}>
    <Order orderId={orderId} me={me} />
  </LiveContext.Provider>
)

let routes: Record<string, () => Response> = {}
let calls: { key: string; init: RequestInit }[] = []
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })
const empty: Page = { messages: [], next: null, follow: null }

beforeEach(() => {
  calls = []
  routes = { [`GET ${PATH}`]: () => json(detail()), [`GET ${PATH}/messages`]: () => json(empty) }
  vi.stubGlobal('fetch', async (url: string, init: RequestInit) => {
    const key = `${init.method} ${url}`
    calls.push({ key, init })
    return (routes[key] ?? (() => json({ error: `no route ${key}` }, 500)))()
  })
})

afterEach(() => {
  vi.unstubAllGlobals()
})

/** Renders the page and lets its first reads answer. */
async function show(me: Me = who('viewer'), state = snapshot()) {
  const rendered = render(page('O/7', me, state))
  await act(async () => {})
  return rendered
}

const field = (name: string) => screen.getByText(name, { selector: 'dt' }).nextElementSibling!.textContent
const buttons = () => screen.queryAllByRole('button').map((b) => b.textContent)

test('shows the order’s fields and requests, and follows its live events', async () => {
  const { rerender } = await show()
  expect(screen.getByRole('heading', { level: 1 }).textContent).toBe('Order O/7')
  expect(field('Status')).toBe('new')
  expect(field('Venue’s OrderID')).toBe('V1')
  expect(field('Side')).toBe('sell short')
  expect(field('Price')).toBe('10.250000000000000001')
  expect(field('Created (UTC)')).toBe('2026-10-14 09:00:07')
  const request = screen.getByText('JB1').closest('tr')!
  expect(request.textContent).toBe('JB1newclientC72026-10-14 09:00:07yes')

  // An event for it changes the fields, and keeps the requests.
  const state = reduce(snapshot([view()]), { type: 'order', ...view({ status: 'partially_filled', cum_qty: '40.5', pending_cancel: true }) } as LiveMessage)
  rerender(page('O/7', who('viewer'), state))
  expect(field('Status')).toBe('partially filledcancel pending')
  expect(field('Filled')).toBe('40.5')
  expect(screen.getByText('JB1')).toBeTruthy()
  const keys = () => calls.map((c) => c.key).sort()
  expect(keys()).toEqual([`GET ${PATH}`, `GET ${PATH}/messages`])

  // A new snapshot reads it again.
  routes[`GET ${PATH}`] = () => json(detail({ status: 'filled', cum_qty: '100' }))
  await act(async () => rerender(page('O/7', who('viewer'), reduce(state, { type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked: [], unavailable: false, clients: null }))))
  expect(keys()).toEqual([`GET ${PATH}`, `GET ${PATH}`, `GET ${PATH}/messages`])
  expect(field('Status')).toBe('filled')
})

test('a missing order says so, with a link back', async () => {
  routes[`GET ${PATH}`] = () => json({ error: 'no order O/7' }, 404)
  await show()
  expect(screen.getByRole('alert').textContent).toBe('No such order.')
  expect(screen.getByRole('link', { name: 'Back to the blotter' }).getAttribute('href')).toBe('#/orders')
})

test("an order out of the user's clients, after a rescope, shows nothing of it", async () => {
  routes[`GET ${PATH}/messages`] = () => json({ messages: [hit('8=FIX.4.4|35=D|11=C7|')], next: null, follow: null })
  const { rerender } = await show()
  expect(screen.getByText('JB1')).toBeTruthy()
  expect(screen.getByText('8=FIX.4.4|35=D|11=C7|')).toBeTruthy()

  routes[`GET ${PATH}`] = () => json({ error: 'not one of your clients' }, 403)
  await act(async () => rerender(page('O/7', who('viewer'), reduce(snapshot(), { type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked: [], unavailable: false, clients: ['FUNDB'] }))))
  expect(screen.getByRole('alert').textContent).toBe('not one of your clients')
  expect(screen.queryByText('JB1')).toBeNull()
  expect(screen.queryByText('8=FIX.4.4|35=D|11=C7|')).toBeNull()
  expect(screen.queryByText('Status')).toBeNull()
  expect(screen.getByRole('link', { name: 'Back to the blotter' })).toBeTruthy()
})

test('messages page on from next, then check for newer from follow', async () => {
  const pages: Page[] = [
    { messages: [hit('8=FIX.4.4|35=D|11=C7|')], next: 'n1', follow: null },
    { messages: [hit('8=FIX.4.4|35=8|', { direction: 'out', msg_type: '8' })], next: null, follow: 'f1' },
    { messages: [hit('8=FIX.4.4|35=F|', { msg_type: 'F' })], next: null, follow: 'f2' },
  ]
  routes[`GET ${PATH}/messages`] = () => json(pages.shift())
  routes[`GET ${PATH}/messages?after=n1`] = routes[`GET ${PATH}/messages`]
  routes[`GET ${PATH}/messages?after=f1`] = routes[`GET ${PATH}/messages`]
  await show()
  expect(screen.queryByRole('button', { name: 'Check for newer' })).toBeNull()
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'More' })))
  expect(screen.queryByRole('button', { name: 'More' })).toBeNull()
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Check for newer' })))
  const rows = [...document.querySelectorAll('table.messages > tbody > tr')]
  expect(rows.map((row) => row.querySelector('code')!.textContent)).toEqual([
    '8=FIX.4.4|35=D|11=C7|',
    '8=FIX.4.4|35=8|',
    '8=FIX.4.4|35=F|',
  ])
  expect(rows[1].textContent).toContain('2026-10-14 09:00:07.123out')
  // The tag=value table, under the raw message.
  const tags = [...rows[0].querySelectorAll('table.tags tr')].map((row) => row.textContent)
  expect(tags).toEqual(['TagValue', '35D', '11C7'])
  expect(screen.getByRole('button', { name: 'Check for newer' })).toBeTruthy()
})

test('message content is shown as text, never as HTML', async () => {
  const evil = '8=FIX.4.4|58=<script>alert(1)</script><img src=x onerror=alert(2)>|'
  routes[`GET ${PATH}/messages`] = () =>
    json({ messages: [hit(evil, { fields: [[58, '<b>bold</b>']], session: '<i>s</i>' })], next: null, follow: null })
  await show()
  expect(document.querySelector('script, img, b, i')).toBeNull()
  expect(screen.getByText(evil).tagName).toBe('CODE')
  expect(screen.getByText('<b>bold</b>')).toBeTruthy()
  expect(screen.getByText('<i>s</i>')).toBeTruthy()
})

test('the actions show by role and state', async () => {
  await show(who('viewer'))
  expect(buttons()).toEqual([])
  for (const role of ['operator', 'admin'] as const) {
    cleanup()
    await show(who(role))
    expect(buttons()).toEqual(['Cancel', 'Ask status'])
  }
  cleanup()

  routes[`GET ${PATH}`] = () => json(detail({ unknown_at_venue: true }))
  await show(who('operator'))
  expect(buttons()).toEqual(['Cancel', 'Ask status', 'Resolve'])
  cleanup()

  routes[`GET ${PATH}`] = () => json(detail({ status: 'canceled', unknown_at_venue: true }))
  await show(who('admin'))
  expect(buttons()).toEqual([])
})

test('an action posts the reason with the CSRF token, reads the order again, and shows a refusal', async () => {
  routes['GET /api/me'] = () => json({ user: 'u', role: 'operator', clients: null, csrf: 'tok' })
  await api.me()
  routes[`GET ${PATH}`] = () => json(detail({ unknown_at_venue: true }))
  await show(who('operator'))

  routes[`POST ${PATH}/resolve`] = () => json({ error: 'the venue hasn’t said it doesn’t know the order' }, 409)
  fireEvent.click(screen.getByRole('button', { name: 'Resolve' }))
  const dialog = screen.getByRole('dialog')
  expect(within(dialog).getByRole('heading').textContent).toBe('Resolve order O/7')
  fireEvent.change(within(dialog).getByLabelText('Reason'), { target: { value: ' venue lost it ' } })
  await act(async () => fireEvent.click(within(dialog).getByRole('button', { name: 'Confirm' })))
  expect(within(dialog).getByRole('alert').textContent).toBe('the venue hasn’t said it doesn’t know the order')

  routes[`POST ${PATH}/resolve`] = () => new Response(null, { status: 204 })
  routes[`GET ${PATH}`] = () => json(detail({ status: 'canceled' }))
  await act(async () => fireEvent.click(within(dialog).getByRole('button', { name: 'Confirm' })))
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(screen.getByRole('status').textContent).toBe('Resolved: the order is canceled.')
  // The Resolve button that opened the dialog is gone: focus goes to the heading.
  expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1 }))
  const posts = calls.filter((c) => c.key.startsWith('POST'))
  expect(posts.map((c) => [c.key, c.init.body, (c.init.headers as Record<string, string>)['X-CSRF-Token']])).toEqual([
    [`POST ${PATH}/resolve`, '{"reason":"venue lost it"}', 'tok'],
    [`POST ${PATH}/resolve`, '{"reason":"venue lost it"}', 'tok'],
  ])
  // Read again: done now, so no more actions.
  expect(calls.filter((c) => c.key === `GET ${PATH}`)).toHaveLength(2)
  expect(field('Status')).toBe('canceled')
  expect(buttons()).toEqual([])
})

test('cancel and status post to their own paths', async () => {
  routes[`POST ${PATH}/cancel`] = () => json({ committed: false }, 202)
  routes[`POST ${PATH}/status`] = () => new Response(null, { status: 202 })
  await show(who('admin'))
  for (const [button, status] of [
    ['Cancel', 'Cancel sent to the venue. Applied; not yet saved to the database.'],
    ['Ask status', 'Status asked of the venue; its answer comes as the order’s reports.'],
  ]) {
    fireEvent.click(screen.getByRole('button', { name: button }))
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'why' } })
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Confirm' })))
    expect(screen.getByRole('status').textContent).toBe(status)
  }
  expect(calls.filter((c) => c.key.startsWith('POST')).map((c) => c.key)).toEqual([`POST ${PATH}/cancel`, `POST ${PATH}/status`])
})

test('an event the store had before the read answered shows over the read’s fields', async () => {
  await show(who('operator'), snapshot([view({ status: 'partially_filled', cum_qty: '40', unknown_at_venue: true })]))
  expect(field('Status')).toBe('partially filledunknown at venue')
  expect(field('Filled')).toBe('40')
  expect(field('Venue’s OrderID')).toBe('V1')
  expect(screen.getByText('JB1')).toBeTruthy()
  expect(buttons()).toEqual(['Cancel', 'Ask status', 'Resolve'])
})

test('an older read answering after a newer one is dropped', async () => {
  const answers: ((order: OrderDetail) => void)[] = []
  vi.stubGlobal('fetch', (url: string) =>
    url === PATH
      ? new Promise<Response>((resolve) => answers.push((order) => resolve(json(order))))
      : Promise.resolve(json(empty)),
  )
  const state = snapshot()
  const { rerender } = render(page('O/7', who('viewer'), state))
  const again = reduce(state, { type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked: [], unavailable: false, clients: null })
  rerender(page('O/7', who('viewer'), again))
  expect(answers).toHaveLength(2)
  await act(async () => answers[1](detail({ status: 'filled' })))
  await act(async () => answers[0](detail({ status: 'new' })))
  expect(field('Status')).toBe('filled')
})

test('messages under StrictMode show once', async () => {
  routes[`GET ${PATH}/messages`] = () => json({ messages: [hit('8=FIX.4.4|35=D|')], next: null, follow: null })
  render(<StrictMode>{page('O/7', who('viewer'), snapshot())}</StrictMode>)
  await act(async () => {})
  expect(document.querySelectorAll('table.messages > tbody > tr')).toHaveLength(1)
})

test('a failed messages read can be retried', async () => {
  routes[`GET ${PATH}/messages`] = () => json({ error: 'no message log' }, 404)
  await show()
  expect(screen.getByRole('alert').textContent).toBe('no message log')
  routes[`GET ${PATH}/messages`] = () => json({ messages: [hit('8=FIX.4.4|35=D|')], next: null, follow: null })
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Retry' })))
  expect(screen.queryByRole('alert')).toBeNull()
  expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull()
  expect(screen.getByText('8=FIX.4.4|35=D|')).toBeTruthy()
})
