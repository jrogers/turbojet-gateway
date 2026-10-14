import { act, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import Routes from './Routes.tsx'
import { initial, LiveContext } from './store.ts'
import type { Me, RouteView } from './types.ts'

const RULES: RouteView[] = [
  { client: null, ex_destination: null, symbol: 'MSFT', venue: null, reject: 'not MSFT' },
  { client: null, ex_destination: 'XNAS', symbol: null, venue: 'NASDAQ', reject: null },
  { client: 'FUNDA', ex_destination: null, symbol: null, venue: 'NASDAQ', reject: null },
]

const who = (clients: string[] | null = null): Me => ({ user: 'u', role: 'viewer', clients })

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

// Fetches no route answers: `request()` would hide the throw as unreachable.
let stray: string[] = []

/** Answers fetches by URL from `routes`, the rules by default, recording them: the screen only GETs. */
function serve(routes: Record<string, () => Response | Promise<Response>> = {}) {
  const calls: string[] = []
  const all: Record<string, () => Response | Promise<Response>> = { '/api/routes': () => json(RULES), ...routes }
  vi.stubGlobal('fetch', async (url: string) => {
    calls.push(url)
    const answer = all[url]
    if (answer === undefined) {
      stray.push(url)
      throw new Error(`unexpected fetch: ${url}`)
    }
    return answer()
  })
  return calls
}

beforeEach(() => {
  serve()
})

afterEach(() => {
  vi.unstubAllGlobals()
  const seen = stray
  stray = []
  expect(seen).toEqual([])
})

const show = (me: Me = who(), reloads = 0) => (
  <LiveContext.Provider value={{ ...initial, link: 'open' }}>
    <Routes me={me} reloads={reloads} />
  </LiveContext.Provider>
)

const rows = () => screen.getAllByRole('row').slice(1)
const dryRun = async (client: string, exDestination: string, symbol: string) => {
  fireEvent.change(screen.getByLabelText('Client (CompID)'), { target: { value: client } })
  fireEvent.change(screen.getByLabelText('ExDestination'), { target: { value: exDestination } })
  fireEvent.change(screen.getByLabelText('Symbol'), { target: { value: symbol } })
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Test' })))
}

test('shows the rules in order: conditions, then the venue or the reject', async () => {
  render(show())
  await screen.findByRole('table')
  expect(rows().map((row) => row.textContent)).toEqual([
    '1anyanyMSFTreject "not MSFT"',
    '2anyXNASanyNASDAQ',
    '3FUNDAanyanyNASDAQ',
  ])
})

test('a dry run names the rule matched, highlighted, and where the order goes', async () => {
  const calls = serve({
    '/api/routes/test?client=FUNDB&ex_destination=XNAS&symbol=AAPL': () =>
      json({ rule: 1, route: RULES[1], target: { venue: 'NASDAQ' } }),
    '/api/routes/test?client=FUNDA&symbol=MSFT': () => json({ rule: 0, route: RULES[0], target: { reject: 'not MSFT' } }),
    '/api/routes/test?client=FUNDB&symbol=AAPL': () => json({ rule: null, route: null, target: null }),
  })
  render(show())
  await screen.findByRole('table')
  const status = screen.getByRole('status')

  await dryRun(' FUNDB ', 'XNAS', 'AAPL')
  expect(status.textContent).toBe('Rule 2 matches: the order would go to NASDAQ.')
  expect(rows().map((row) => row.className)).toEqual(['', 'matched', ''])
  expect(rows()[1].getAttribute('aria-current')).toBe('true')

  await dryRun('FUNDA', '', 'MSFT')
  expect(status.textContent).toBe('Rule 1 matches: the order would be rejected "not MSFT".')
  expect(rows().map((row) => row.className)).toEqual(['matched', '', ''])

  await dryRun('FUNDB', '', 'AAPL')
  expect(status.textContent).toBe('No route matches: the order would be rejected "no route for this order".')
  expect(screen.getByRole('table').querySelector('.matched')).toBeNull()
  expect(calls.filter((url) => url.startsWith('/api/routes/test'))).toHaveLength(3)
})

test("a scoped user's dry run of another client shows the API's refusal, and clears the last answer", async () => {
  serve({
    '/api/routes/test?client=FUNDA&symbol=AAPL': () => json({ rule: 2, route: RULES[2], target: { venue: 'NASDAQ' } }),
    '/api/routes/test?client=FUNDB&symbol=AAPL': () => json({ error: 'u may see only clients FUNDA, not FUNDB' }, 403),
  })
  render(show(who(['FUNDA'])))
  await screen.findByRole('table')
  // Its clients are suggested.
  expect(Array.from(document.querySelectorAll('#route-clients option')).map((o) => o.getAttribute('value'))).toEqual(['FUNDA'])
  await dryRun('FUNDA', '', 'AAPL')
  expect(screen.getByRole('status').textContent).toBe('Rule 3 matches: the order would go to NASDAQ.')

  await dryRun('FUNDB', '', 'AAPL')
  expect(screen.getByRole('alert').textContent).toBe('u may see only clients FUNDA, not FUNDB')
  expect(screen.getByRole('status').textContent).toBe('')
  expect(screen.getByRole('table').querySelector('.matched')).toBeNull()
})

test('client and symbol are required, asking nothing without them', async () => {
  const calls = serve()
  render(show())
  await screen.findByRole('table')
  fireEvent.change(screen.getByLabelText('Client (CompID)'), { target: { value: 'FUNDA' } })
  fireEvent.submit(screen.getByRole('button', { name: 'Test' }).closest('form')!)
  expect(screen.getByRole('alert').textContent).toBe('Client and symbol are required.')
  expect(calls).toEqual(['/api/routes'])
})

test("an earlier dry run's late answer is dropped", async () => {
  let late = (_: Response) => {}
  serve({
    '/api/routes/test?client=FUNDA&symbol=MSFT': () => new Promise<Response>((resolve) => (late = resolve)),
    '/api/routes/test?client=FUNDA&symbol=AAPL': () => json({ rule: 2, route: RULES[2], target: { venue: 'NASDAQ' } }),
  })
  render(show())
  await screen.findByRole('table')
  await dryRun('FUNDA', '', 'MSFT')
  await dryRun('FUNDA', '', 'AAPL')
  await act(async () => late(json({ rule: 0, route: RULES[0], target: { reject: 'not MSFT' } })))
  expect(screen.getByRole('status').textContent).toBe('Rule 3 matches: the order would go to NASDAQ.')
})

test('the rules are read again after a reload, and the last answer goes', async () => {
  const calls = serve({
    '/api/routes/test?client=FUNDA&symbol=AAPL': () => json({ rule: 2, route: RULES[2], target: { venue: 'NASDAQ' } }),
  })
  const { rerender } = render(show())
  await screen.findByRole('table')
  await dryRun('FUNDA', '', 'AAPL')
  serve({ '/api/routes': () => json(RULES.slice(0, 1)) })
  await act(async () => rerender(show(who(), 1)))
  expect(rows()).toHaveLength(1)
  expect(screen.getByRole('status').textContent).toBe('')
  expect(calls).toEqual(['/api/routes', '/api/routes/test?client=FUNDA&symbol=AAPL'])
})

test('a refused read of the rules says why and shows none', async () => {
  serve({ '/api/routes': () => json({ error: 'nope' }, 403) })
  render(show())
  expect((await screen.findByRole('alert')).textContent).toBe('nope')
  expect(screen.queryByRole('table')).toBeNull()
  expect(within(screen.getByRole('region', { name: 'Dry run' })).getByRole('button', { name: 'Test' })).toBeTruthy()
})
