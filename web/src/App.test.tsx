import { StrictMode } from 'react'
import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import App from './App.tsx'
import * as api from './api.ts'
import { FakeSocket } from './fake-socket.ts'
import type { Me } from './types.ts'

const alice: Me = { user: 'alice', role: 'operator', clients: ['FUNDA', 'FUNDB'], csrf: 'tok' }

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })

// Fetches no route answers: `request()` would hide the throw as unreachable.
let stray: string[] = []

/** Answers fetches by `METHOD path` from `routes`, recording the requests. */
function serve(routes: Record<string, () => Response>) {
  const calls: string[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit) => {
    const key = `${init.method} ${url}`
    calls.push(key)
    const answer = routes[key]
    if (answer === undefined) {
      stray.push(key)
      throw new Error(`unexpected fetch: ${key}`)
    }
    return answer()
  })
  return calls
}

/**
 * The live socket opened last, once one is: the shell's text can show before its effects have
 * run, which open it.
 */
async function opened(): Promise<FakeSocket> {
  await waitFor(() => expect(FakeSocket.opened).not.toHaveLength(0))
  return FakeSocket.opened[FakeSocket.opened.length - 1]
}

beforeEach(() => {
  window.location.hash = ''
  FakeSocket.opened = []
  // The shell opens the live socket: a fake, which nothing answers unless a test says.
  vi.stubGlobal('WebSocket', FakeSocket)
})

afterEach(() => {
  vi.unstubAllGlobals()
  const seen = stray
  stray = []
  expect(seen).toEqual([])
})

test('shows login without a session, and the app once logged in', async () => {
  let password = ''
  serve({
    'GET /api/me': () => json({ error: 'log in' }, 401),
    'POST /api/login': () => (password === 'right' ? json(alice) : json({ error: 'wrong user or password' }, 401)),
  })
  render(<App />)
  const user = await screen.findByLabelText('User')
  // No session at start is no session ended: nothing to say.
  expect(screen.queryByRole('alert')).toBeNull()
  expect(document.activeElement).toBe(user)
  fireEvent.change(user, { target: { value: 'alice' } })

  password = 'wrong'
  fireEvent.change(screen.getByLabelText('Password'), { target: { value: password } })
  fireEvent.click(screen.getByRole('button', { name: 'Log in' }))
  expect((await screen.findByRole('alert')).textContent).toBe('wrong user or password')

  password = 'right'
  fireEvent.change(screen.getByLabelText('Password'), { target: { value: password } })
  fireEvent.click(screen.getByRole('button', { name: 'Log in' }))
  expect(await screen.findByRole('button', { name: 'Log out' })).toBeTruthy()
})

test('the shell shows the role and clients, or all clients, and logs out', async () => {
  const calls = serve({ 'GET /api/me': () => json(alice), 'POST /api/logout': () => new Response(null, { status: 204 }) })
  const { unmount } = render(<App />)
  await screen.findByText('operator')
  expect(screen.getByText('FUNDA, FUNDB')).toBeTruthy()

  fireEvent.click(screen.getByRole('button', { name: 'Log out' }))
  await screen.findByLabelText('User')
  expect(calls).toContain('POST /api/logout')
  unmount()

  serve({ 'GET /api/me': () => json({ user: 'root', role: 'admin', clients: null }) })
  render(<App />)
  await screen.findByText('admin')
  expect(screen.getByText('all clients')).toBeTruthy()
})

test('routes switch with the hash, orders by default', async () => {
  serve({
    'GET /api/me': () => json(alice),
    'GET /api/sessions': () => json([]),
    'GET /api/risk': () => json({ error: 'not now' }, 503),
    'GET /api/risk/limits': () => json({ defaults: {}, clients: {} }),
    'GET /api/routes': () => json([]),
    'GET /api/orders/A%2F1': () => json({ error: 'no order A/1' }, 404),
    'GET /api/orders/A%2F1/messages': () => json({ error: 'no order A/1' }, 404),
  })
  render(<App />)
  expect((await screen.findByRole('heading')).textContent).toBe('Orders')

  // The shell's commit can be in the DOM before its effects have run, so its hashchange listener
  // may not be there yet: useSyncExternalStore then sees the new hash as it subscribes, a moment
  // later. So each navigation waits for its screen rather than expecting it at once.
  const go = (hash: string) =>
    act(() => {
      window.location.hash = hash
      window.dispatchEvent(new HashChangeEvent('hashchange'))
    })
  go('#/sessions')
  await screen.findByRole('heading', { level: 1, name: 'Sessions' })
  expect(screen.getByRole('link', { name: 'Sessions' }).getAttribute('aria-current')).toBe('page')
  go('#/risk')
  await screen.findByRole('heading', { level: 1, name: 'Risk' })
  expect(screen.getByRole('link', { name: 'Risk' }).getAttribute('aria-current')).toBe('page')
  expect(await screen.findByText('not now')).toBeTruthy()
  // Nothing read until a search is asked.
  go('#/messages')
  await screen.findByRole('heading', { level: 1, name: 'Messages' })
  expect(screen.getByRole('link', { name: 'Messages' }).getAttribute('aria-current')).toBe('page')
  go('#/routes')
  await screen.findByRole('heading', { level: 1, name: 'Routes' })
  expect(screen.getByRole('link', { name: 'Routes' }).getAttribute('aria-current')).toBe('page')
  expect(await screen.findByText('No routes: every order is rejected.')).toBeTruthy()
  go('#/orders/A%2F1')
  await screen.findByRole('heading', { level: 1, name: 'Order A/1' })
  expect(within(screen.getByRole('navigation')).getByRole('link', { name: 'Orders' }).getAttribute('aria-current')).toBe('page')
  expect(await screen.findByText('No such order.')).toBeTruthy()
  go('#/nowhere')
  await screen.findByRole('heading', { level: 1, name: 'Orders' })
})

test('a logout that fails keeps the shell and says why', async () => {
  let status = 500
  serve({
    'GET /api/me': () => json(alice),
    'POST /api/logout': () => (status === 204 ? new Response(null, { status }) : json({ error: 'cannot write the audit log' }, status)),
  })
  render(<App />)
  const button = await screen.findByRole('button', { name: 'Log out' })
  fireEvent.click(button)
  expect((await screen.findByRole('alert')).textContent).toBe('cannot write the audit log')
  expect(screen.getByText('operator')).toBeTruthy()
  expect((button as HTMLButtonElement).disabled).toBe(false)

  status = 204
  fireEvent.click(button)
  await screen.findByLabelText('User')
  expect(screen.queryByRole('alert')).toBeNull()
})

test('a 401 later returns to login, saying the session ended, until the next login', async () => {
  serve({
    'GET /api/me': () => json(alice),
    'GET /api/sessions': () => json({ error: 'the session has ended: log in again' }, 401),
    'POST /api/login': () => json(alice),
  })
  render(<App />)
  await screen.findByText('operator')
  await act(() => api.sessions().catch(() => {}))
  expect(screen.getByRole('alert').textContent).toBe('Your session has ended; log in again.')

  fireEvent.change(screen.getByLabelText('User'), { target: { value: 'alice' } })
  fireEvent.change(screen.getByLabelText('Password'), { target: { value: 'right' } })
  fireEvent.click(screen.getByRole('button', { name: 'Log in' }))
  await screen.findByText('operator')
  expect(screen.queryByRole('alert')).toBeNull()
})

test('says so when the gateway cannot be reached at start', async () => {
  vi.stubGlobal('fetch', async () => {
    throw new TypeError('Failed to fetch')
  })
  render(<App />)
  expect((await screen.findByRole('alert')).textContent).toBe('cannot reach the gateway')
})

test('the shell keeps the live socket while logged in, and says when it is down', async () => {
  serve({
    'GET /api/me': () => json(alice),
    'GET /api/sessions': () => json([]),
    'POST /api/logout': () => new Response(null, { status: 204 }),
  })
  window.location.hash = '#/sessions'
  render(<App />)
  await screen.findByText('operator')
  const socket = await opened()
  act(() => {
    socket.open()
    socket.send({ type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked: [], unavailable: false, clients: null })
  })
  expect(screen.getByRole('heading', { name: 'Clients' })).toBeTruthy()
  expect(screen.queryByText('Disconnected, reconnecting…')).toBeNull()

  fireEvent.click(screen.getByRole('button', { name: 'Log out' }))
  await screen.findByLabelText('User')
  expect(socket.closed).toBe(true)
})

test('a live socket lost shows a banner until it opens again', async () => {
  serve({ 'GET /api/me': () => json(alice) })
  render(<App />)
  await screen.findByText('operator')
  const socket = await opened()
  act(() => socket.open())
  act(() => socket.end(1001, 'the gateway is stopping'))
  expect(screen.getByRole('status').textContent).toBe('Disconnected, reconnecting…')
  // The reconnect, a second or so later.
  await waitFor(() => expect(FakeSocket.opened.length).toBeGreaterThan(1), { timeout: 2_000 })
  act(() => FakeSocket.opened[FakeSocket.opened.length - 1].open())
  expect(screen.queryByRole('status')).toBeNull()
})

test('a live socket refused by policy returns to login, saying why', async () => {
  serve({ 'GET /api/me': () => json(alice) })
  render(<App />)
  await screen.findByText('operator')
  const socket = await opened()
  act(() => {
    socket.open()
    socket.end(1008, 'the session has ended: log in again')
  })
  expect(screen.getByRole('alert').textContent).toBe('the session has ended: log in again')
  expect(screen.getByLabelText('User')).toBeTruthy()
})

test('under StrictMode the shell leaves one live socket open', async () => {
  serve({ 'GET /api/me': () => json(alice) })
  render(
    <StrictMode>
      <App />
    </StrictMode>,
  )
  await screen.findByText('operator')
  // Mounted, unmounted and mounted again: two opened, the first closed.
  await waitFor(() => expect(FakeSocket.opened).toHaveLength(2))
  expect(FakeSocket.opened.filter((socket) => !socket.closed)).toHaveLength(1)
})

test('alarms show newest first, counted in the header, until dismissed; a snapshot keeps them', async () => {
  serve({ 'GET /api/me': () => json(alice) })
  render(<App />)
  await screen.findByText('operator')
  const socket = await opened()
  const snapshot = { type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked: [], unavailable: false, clients: null }
  act(() => {
    socket.open()
    socket.send(snapshot)
  })
  expect(screen.queryByRole('region', { name: 'Alarms' })).toBeNull()
  expect(screen.queryByText(/alarms?$/)).toBeNull()

  const focused = document.activeElement
  act(() => {
    socket.send({ type: 'alarm', time: '2026-10-14T09:30:01.123Z', kind: 'stuck_cancel', client: 'FUNDA', text: 'gave up cancelling 7' })
    socket.send({ type: 'alarm', time: '2026-10-14T09:31:02Z', kind: 'venue_store_reset', client: null, text: 'XNYS reset its store' })
  })
  // A new alarm doesn't take the focus.
  expect(document.activeElement).toBe(focused)
  // Announced, from a region there before it came.
  expect(screen.getByText('2 alarms').parentElement!.getAttribute('aria-live')).toBe('polite')
  const items = () =>
    Array.from(screen.getByRole('region', { name: 'Alarms' }).querySelectorAll('li')).map((li) => li.textContent)
  expect(items()).toEqual([
    '2026-10-14 09:31:02 venue store reset XNYS reset its store Dismiss',
    '2026-10-14 09:30:01 stuck cancel FUNDA gave up cancelling 7 Dismiss',
  ])

  act(() => socket.send(snapshot))
  expect(items()).toHaveLength(2)

  fireEvent.click(screen.getByRole('button', { name: 'Dismiss the venue store reset alarm of 2026-10-14 09:31:02' }))
  expect(items()).toEqual(['2026-10-14 09:30:01 stuck cancel FUNDA gave up cancelling 7 Dismiss'])
  expect(screen.getByText('1 alarm')).toBeTruthy()

  // Dismissed stays dismissed as more come.
  act(() => socket.send({ type: 'alarm', time: '2026-10-14T09:32:00Z', kind: 'orphan', client: 'FUNDB', text: 'no order for 9' }))
  expect(items()).toHaveLength(2)
  expect(screen.getByText('2 alarms')).toBeTruthy()

  fireEvent.click(screen.getByRole('button', { name: /^Dismiss the orphan/ }))
  fireEvent.click(screen.getByRole('button', { name: /^Dismiss the stuck cancel/ }))
  expect(screen.queryByRole('region', { name: 'Alarms' })).toBeNull()
  expect(screen.queryByText(/alarms?$/)).toBeNull()
})

test('the gateway unavailable shows a banner until it is available, from an alarm or a snapshot', async () => {
  serve({ 'GET /api/me': () => json(alice) })
  render(<App />)
  await screen.findByText('operator')
  const socket = await opened()
  const snapshot = (unavailable: boolean) => ({ type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked: [], unavailable, clients: null })
  act(() => {
    socket.open()
    socket.send(snapshot(false))
  })
  expect(screen.queryByRole('alert')).toBeNull()

  act(() => socket.send({ type: 'alarm', time: '2026-10-14T09:30:00Z', kind: 'gateway_unavailable', client: null, text: 'database behind' }))
  expect(screen.getByRole('alert').textContent).toMatch(/^The gateway is unavailable/)
  // Dismissing the alarm leaves the banner: it says how things stand, not what happened.
  fireEvent.click(screen.getByRole('button', { name: /^Dismiss the gateway unavailable/ }))
  expect(screen.getByRole('alert')).toBeTruthy()

  act(() => socket.send({ type: 'alarm', time: '2026-10-14T09:30:05Z', kind: 'gateway_available', client: null, text: 'caught up' }))
  expect(screen.queryByRole('alert')).toBeNull()

  act(() => socket.send(snapshot(true)))
  expect(screen.getByRole('alert')).toBeTruthy()
  act(() => socket.send(snapshot(false)))
  expect(screen.queryByRole('alert')).toBeNull()
})

test('a snapshot under other clients shows them in the header, and scopes the actions', async () => {
  serve({ 'GET /api/me': () => json(alice), 'GET /api/sessions': () => json([]) })
  window.location.hash = '#/sessions'
  render(<App />)
  await screen.findByText('FUNDA, FUNDB')
  const socket = await opened()
  const venue = {
    side: 'venue',
    id: 'FIX.4.4:JETBRIDGE->NASDAQ',
    venue: 'NASDAQ',
    status: 'logged_on',
    next_incoming: null,
    next_outgoing: null,
    activity: null,
    paused: false,
  }
  const snapshot = (clients: string[] | null) => ({ type: 'snapshot', sessions: [venue], orders: [], orders_next: null, blocked: [], unavailable: false, clients })
  act(() => {
    socket.open()
    socket.send(snapshot(['FUNDA']))
  })
  expect(screen.getByText('FUNDA')).toBeTruthy()
  // Scoped: a venue isn't its to act on.
  expect(screen.queryByRole('button', { name: 'Pause' })).toBeNull()
  act(() => socket.send(snapshot(null)))
  expect(screen.getByText('all clients')).toBeTruthy()
  expect(screen.getByRole('button', { name: 'Pause' })).toBeTruthy()
})

const root: Me = { user: 'root', role: 'admin', clients: null, csrf: 'tok' }
const links = () => within(screen.getByRole('navigation')).getAllByRole('link').map((link) => link.textContent)

test('the nav shows Routes to all, and Audit and Reload to an admin without clients only', async () => {
  serve({ 'GET /api/me': () => json(alice) })
  const { unmount } = render(<App />)
  await screen.findByText('operator')
  expect(links()).toEqual(['Orders', 'Sessions', 'Risk', 'Messages', 'Routes'])
  expect(screen.queryByRole('button', { name: 'Reload…' })).toBeNull()
  unmount()

  serve({ 'GET /api/me': () => json({ ...root, clients: ['FUNDA'] }) })
  render(<App />)
  await screen.findByText('admin')
  expect(links()).toEqual(['Orders', 'Sessions', 'Risk', 'Messages', 'Routes'])
  expect(screen.queryByRole('button', { name: 'Reload…' })).toBeNull()
  // A snapshot saying it has every client now shows them; one scoping it hides them again.
  const socket = await opened()
  const snapshot = (clients: string[] | null) => ({ type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked: [], unavailable: false, clients })
  act(() => {
    socket.open()
    socket.send(snapshot(null))
  })
  expect(links()).toEqual(['Orders', 'Sessions', 'Risk', 'Messages', 'Routes', 'Audit'])
  expect(screen.getByRole('button', { name: 'Reload…' })).toBeTruthy()
  act(() => socket.send(snapshot(['FUNDA'])))
  expect(links()).not.toContain('Audit')
  expect(screen.queryByRole('button', { name: 'Reload…' })).toBeNull()
})

test('the audit log by its hash is not available to a user it is hidden from, asking nothing', async () => {
  const calls = serve({ 'GET /api/me': () => json({ ...root, clients: ['FUNDA'] }) })
  window.location.hash = '#/audit'
  render(<App />)
  await screen.findByRole('heading', { level: 1, name: 'Audit' })
  expect(screen.getByText('Not available to you.')).toBeTruthy()
  expect(calls).toEqual(['GET /api/me'])
})

test('an admin reloads from the header, and sees what it took or why a file was kept', async () => {
  let answer = () =>
    json({
      sessions: { added: ['FUNDC'], changed: [], removed: [], logged_out: [], started: [], reconfigured: [], stopped: [] },
      config: { routes: 3, clients: 1, day_end: '17:00' },
      warnings: ['[database] url changed: restart to use it'],
    })
  const calls = serve({
    'GET /api/me': () => json(root),
    'GET /api/audit?limit=100': () => json([]),
    'POST /api/reload': () => answer(),
  })
  window.location.hash = '#/audit'
  render(<App />)
  await screen.findByText('None.')
  const reload = async (reason: string) => {
    fireEvent.click(screen.getByRole('button', { name: 'Reload…' }))
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: reason } })
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Reload' })))
  }
  const outcome = () => document.querySelector('.reloaded [role=status]')!
  expect(outcome().textContent).toBe('')

  await reload('new client')
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(outcome().textContent).toBe(
    'The reload took 3 routes, limits for 1 clients and day_end 17:00; sessions: added FUNDC.[database] url changed: restart to use it',
  )
  expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1, name: 'Audit' }))

  answer = () => json({ error: 'jetbridge.toml is kept: bad route; the sessions file: nothing changed' }, 422)
  await reload('again')
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(outcome().textContent).toBe(
    'The reload kept a file: jetbridge.toml is kept: bad route; the sessions file: nothing changed',
  )

  // Any other failure stays in the dialog, to try again.
  answer = () => json({ error: 'the gateway is stopping' }, 503)
  await reload('once more')
  expect(within(screen.getByRole('dialog')).getByRole('alert').textContent).toBe('the gateway is stopping')
  fireEvent.click(screen.getByRole('button', { name: 'Cancel' }))

  fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }))
  expect(outcome().textContent).toBe('')
  expect(calls.filter((call) => call === 'POST /api/reload')).toHaveLength(3)
})
