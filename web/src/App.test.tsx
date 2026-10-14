import { StrictMode } from 'react'
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react'
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
    'GET /api/orders/A%2F1': () => json({ error: 'no order A/1' }, 404),
    'GET /api/orders/A%2F1/messages': () => json({ error: 'no order A/1' }, 404),
  })
  render(<App />)
  expect((await screen.findByRole('heading')).textContent).toBe('Orders')

  const go = (hash: string) =>
    act(() => {
      window.location.hash = hash
      window.dispatchEvent(new HashChangeEvent('hashchange'))
    })
  go('#/sessions')
  expect(screen.getByRole('heading').textContent).toBe('Sessions')
  expect(screen.getByRole('link', { name: 'Sessions' }).getAttribute('aria-current')).toBe('page')
  go('#/orders/A%2F1')
  expect(screen.getByRole('heading', { level: 1 }).textContent).toBe('Order A/1')
  expect(await screen.findByText('No such order.')).toBeTruthy()
  go('#/nowhere')
  expect(screen.getByRole('heading').textContent).toBe('Orders')
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
    socket.send({ type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked: [], unavailable: false })
  })
  expect(screen.getByRole('heading', { name: 'Clients' })).toBeTruthy()
  expect(screen.queryByRole('status')).toBeNull()

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
