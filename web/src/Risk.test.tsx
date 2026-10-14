import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, expect, test, vi } from 'vitest'
import * as api from './api.ts'
import Risk, { RISK_EVERY } from './Risk.tsx'
import { initial, LiveContext, reduce, type LiveState } from './store.ts'
import type { Limits, LiveMessage, Me, RiskView } from './types.ts'

const FUNDA = 'FIX.4.4:JETBRIDGE->FUNDA'
const FUNDB = 'FIX.4.4:JETBRIDGE->FUNDB/desk'

const none: Limits = { max_order_qty: null, max_notional: null, symbols: null, price_band: null, credit: null, self_trade: false }

const fixture: RiskView = {
  defaults: { ...none, max_order_qty: '1000', price_band: '5.5' },
  by_comp_id: { FUNDB: { ...none, max_order_qty: '1000', price_band: '5.5', credit: '250000.50', symbols: ['AAPL', 'MSFT'], self_trade: true } },
  sessions: [
    { client: FUNDA, open: '1025.0000', filled: '0' },
    { client: FUNDB, open: '0.000000000000000001', filled: '12.5' },
  ],
  references: { AAPL: '10.250000000000000001' },
  blocked: [],
}

const snapshot = (blocked: string[] = []): LiveState =>
  reduce(initial, { type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked, unavailable: false, clients: null })

const who = (role: Me['role'], clients: string[] | null = null): Me => ({ user: 'u', role, clients })

const page = (me: Me, state: LiveState) => (
  <LiveContext.Provider value={{ ...state, link: 'open' }}>
    <Risk me={me} />
  </LiveContext.Provider>
)

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

// Fetches no route answers: `request()` would hide the throw as unreachable.
let stray: string[] = []

/** Answers fetches by `METHOD path` from `routes`, recording them; any other fails the test. */
function serve(routes: Record<string, () => Response>) {
  const calls: { key: string; init: RequestInit }[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit) => {
    const key = `${init.method} ${url}`
    calls.push({ key, init })
    const answer = routes[key]
    if (answer === undefined) {
      stray.push(key)
      throw new Error(`unexpected fetch: ${key}`)
    }
    return answer()
  })
  return calls
}

afterEach(() => {
  vi.unstubAllGlobals()
  const seen = stray
  stray = []
  expect(seen).toEqual([])
})

const row = (name: string) => screen.getByText(name).closest('tr')!
const cells = (name: string) => [...row(name).querySelectorAll('td')].map((td) => td.textContent)

test('shows each session against its limits as merged, and the reference prices, as written', async () => {
  serve({ 'GET /api/risk': () => json(fixture) })
  render(page(who('viewer'), snapshot()))
  await screen.findByText(FUNDA)
  // FUNDA has none of its own: the defaults. FUNDB's session, with a SubID, has FUNDB's.
  expect(cells(FUNDA)).toEqual([FUNDA, '1025.0000', '0', '', '', '1000', '5.5', '', ''])
  expect(cells(FUNDB)).toEqual([FUNDB, '0.000000000000000001', '12.5', '250000.50', '', '1000', '5.5', 'AAPL, MSFT', 'refused'])
  expect(cells('AAPL')).toEqual(['AAPL', '10.250000000000000001'])
})

test('a 403 clears what was shown, and says why', async () => {
  let answer = () => json(fixture)
  serve({ 'GET /api/risk': () => answer() })
  const { rerender } = render(page(who('viewer'), snapshot()))
  await screen.findByText(FUNDA)
  answer = () => json({ error: 'u may see only clients FUNDA' }, 403)
  // A snapshot reads again.
  rerender(page(who('viewer'), reduce(snapshot(), { ...snapshotMessage(), clients: ['FUNDA'] })))
  expect((await screen.findByRole('alert')).textContent).toBe('u may see only clients FUNDA')
  expect(screen.queryByText(FUNDA)).toBeNull()
  expect(screen.queryAllByRole('table')).toEqual([])
})

const snapshotMessage = (): Extract<LiveMessage, { type: 'snapshot' }> => ({
  type: 'snapshot',
  sessions: [],
  orders: [],
  orders_next: null,
  blocked: [],
  unavailable: false,
  clients: null,
})

test('reads on showing, then every few seconds while seen, and not once gone', async () => {
  vi.useFakeTimers()
  try {
    const calls = serve({ 'GET /api/risk': () => json(fixture) })
    let hidden = false
    vi.spyOn(document, 'hidden', 'get').mockImplementation(() => hidden)
    const { unmount } = render(page(who('viewer'), snapshot()))
    await act(() => vi.advanceTimersByTimeAsync(0))
    expect(calls).toHaveLength(1)
    await act(() => vi.advanceTimersByTimeAsync(RISK_EVERY))
    expect(calls).toHaveLength(2)

    hidden = true
    await act(() => vi.advanceTimersByTimeAsync(RISK_EVERY * 3))
    expect(calls).toHaveLength(2)
    hidden = false
    await act(() => vi.advanceTimersByTimeAsync(RISK_EVERY))
    expect(calls).toHaveLength(3)

    unmount()
    await vi.advanceTimersByTimeAsync(RISK_EVERY * 3)
    expect(calls).toHaveLength(3)
  } finally {
    vi.useRealTimers()
    vi.restoreAllMocks()
  }
})

test('a read answering after a newer one was asked is dropped', async () => {
  const answers: ((r: Response) => void)[] = []
  vi.stubGlobal('fetch', () => new Promise<Response>((resolve) => answers.push(resolve)))
  const { rerender } = render(page(who('viewer'), snapshot()))
  rerender(page(who('viewer'), reduce(snapshot(), snapshotMessage())))
  expect(answers).toHaveLength(2)
  await act(async () => answers[1](json({ ...fixture, sessions: [fixture.sessions[1]] })))
  await act(async () => answers[0](json(fixture)))
  expect(screen.queryByText(FUNDA)).toBeNull()
  expect(screen.getByText(FUNDB)).toBeTruthy()
})

test('the blocks follow the store', async () => {
  serve({ 'GET /api/risk': () => json(fixture) })
  let state = snapshot(['comp:FUNDA'])
  const { rerender } = render(page(who('viewer'), state))
  const blocks = () => [...document.querySelectorAll('ul.blocks li')].map((li) => li.textContent)
  expect(blocks()).toEqual(['comp:FUNDA'])
  state = reduce(state, { type: 'blocked', scope: 'all' })
  rerender(page(who('viewer'), state))
  expect(blocks()).toEqual(['comp:FUNDA', 'all'])
  state = reduce(state, { type: 'unblocked', scope: 'comp:FUNDA' })
  rerender(page(who('viewer'), state))
  expect(blocks()).toEqual(['all'])
  state = reduce(state, { type: 'unblocked', scope: 'all' })
  rerender(page(who('viewer'), state))
  expect(screen.getByText('None.')).toBeTruthy()
  await screen.findByText(FUNDA)
})

test('the actions show by role, and a scoped user is offered only its CompIDs', async () => {
  serve({ 'GET /api/risk': () => json(fixture) })
  const buttons = () => screen.queryAllByRole('button').map((b) => b.textContent)
  const offered = () => [...document.querySelectorAll('datalist option')].map((o) => (o as HTMLOptionElement).value)

  render(page(who('viewer'), snapshot(['all'])))
  await screen.findByText(FUNDA)
  expect(buttons()).toEqual([])
  cleanup()

  render(page(who('operator'), snapshot(['all'])))
  await screen.findByText(FUNDA)
  expect(buttons()).toEqual(['Unblock', 'Block', 'Cancel all'])
  expect(screen.getByRole('button', { name: 'Cancel all' }).className).toBe('danger')
  expect(offered()).toEqual(['all', 'comp:FUNDB'])
  cleanup()

  render(page(who('admin', ['FUNDA']), snapshot(['all', 'comp:FUNDA'])))
  await screen.findByText(FUNDA)
  expect(buttons()).toEqual(['Unblock', 'Block', 'Cancel all'])
  expect(within(screen.getByText('comp:FUNDA').closest('li')!).getByRole('button').textContent).toBe('Unblock')
  expect(offered()).toEqual(['comp:FUNDA'])
})

test('block and unblock post the reason and scope with the CSRF token, and say how it went', async () => {
  serve({ 'GET /api/me': () => json({ user: 'u', role: 'operator', clients: null, csrf: 'tok' }) })
  await api.me()
  let changed = true
  const calls = serve({
    'GET /api/risk': () => json(fixture),
    'POST /api/risk/block': () => json({ changed }),
    'POST /api/risk/unblock': () => json({ changed }),
  })
  render(page(who('operator'), snapshot(['comp:FUNDA'])))
  await screen.findByText(FUNDA)

  fireEvent.change(screen.getByLabelText('Scope'), { target: { value: ' session:X ' } })
  fireEvent.click(screen.getByRole('button', { name: 'Block' }))
  const dialog = screen.getByRole('dialog')
  expect(within(dialog).getByRole('heading').textContent).toBe('Block session:X')
  fireEvent.change(within(dialog).getByLabelText('Reason'), { target: { value: 'runaway' } })
  await act(async () => fireEvent.click(within(dialog).getByRole('button', { name: 'Confirm' })))
  expect(screen.getByRole('status').textContent).toBe('Blocked session:X.')
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1 }))

  changed = false
  fireEvent.click(screen.getByRole('button', { name: 'Unblock' }))
  fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'calm' } })
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Confirm' })))
  expect(screen.getByRole('status').textContent).toBe("comp:FUNDA wasn't blocked.")

  const posts = calls.filter((c) => c.init.method === 'POST')
  expect(posts.map((c) => [c.key, c.init.body])).toEqual([
    ['POST /api/risk/block', '{"reason":"runaway","scope":"session:X"}'],
    ['POST /api/risk/unblock', '{"reason":"calm","scope":"comp:FUNDA"}'],
  ])
  for (const post of posts) expect((post.init.headers as Record<string, string>)['X-CSRF-Token']).toBe('tok')
})

test('the kill switch needs its scope typed exactly, then posts', async () => {
  let status = 403
  const calls = serve({
    'GET /api/risk': () => json(fixture),
    'POST /api/risk/cancel-all': () =>
      status === 204 ? new Response(null, { status }) : json({ error: 'u may act only on clients FUNDA, not all' }, status),
  })
  render(page(who('operator'), snapshot()))
  await screen.findByText(FUNDA)
  fireEvent.change(screen.getByLabelText('Scope'), { target: { value: 'comp:FUNDA' } })
  fireEvent.click(screen.getByRole('button', { name: 'Cancel all' }))
  const dialog = screen.getByRole('dialog')
  expect(within(dialog).getByRole('heading').textContent).toBe('Cancel all orders of comp:FUNDA')
  const confirm = within(dialog).getByRole('button', { name: 'Confirm' }) as HTMLButtonElement
  fireEvent.change(within(dialog).getByLabelText('Reason'), { target: { value: 'runaway algo' } })
  expect(confirm.disabled).toBe(true)
  const typed = within(dialog).getByLabelText('Type comp:FUNDA to confirm')
  fireEvent.change(typed, { target: { value: 'comp:FUND' } })
  expect(confirm.disabled).toBe(true)
  // Enter in a field doesn't get past it either.
  fireEvent.submit(dialog.querySelector('form')!)
  expect(calls.filter((c) => c.init.method === 'POST')).toEqual([])
  fireEvent.change(typed, { target: { value: 'comp:FUNDA' } })
  expect(confirm.disabled).toBe(false)

  // A refusal stays open, saying why.
  await act(async () => fireEvent.click(confirm))
  expect(within(dialog).getByRole('alert').textContent).toBe('u may act only on clients FUNDA, not all')

  status = 204
  await act(async () => fireEvent.click(confirm))
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(screen.getByRole('status').textContent).toMatch(/^Blocked comp:FUNDA and sent cancels/)
  const posts = calls.filter((c) => c.init.method === 'POST')
  expect(posts).toHaveLength(2)
  expect(posts[1].key).toBe('POST /api/risk/cancel-all')
  expect(posts[1].init.body).toBe('{"reason":"runaway algo","scope":"comp:FUNDA"}')
})
