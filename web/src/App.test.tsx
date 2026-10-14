import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import App from './App.tsx'
import type { Me } from './types.ts'

const alice: Me = { user: 'alice', role: 'operator', clients: ['FUNDA', 'FUNDB'], csrf: 'tok' }

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })

/** Answers fetches by `METHOD path` from `routes`, recording the requests. */
function serve(routes: Record<string, () => Response>) {
  const calls: string[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit) => {
    const key = `${init.method} ${url}`
    calls.push(key)
    return routes[key]?.() ?? json({ error: 'not found' }, 404)
  })
  return calls
}

beforeEach(() => {
  window.location.hash = ''
})

afterEach(() => vi.unstubAllGlobals())

test('shows login without a session, and the app once logged in', async () => {
  let password = ''
  serve({
    'GET /api/me': () => json({ error: 'log in' }, 401),
    'POST /api/login': () => (password === 'right' ? json(alice) : json({ error: 'wrong user or password' }, 401)),
  })
  render(<App />)
  const user = await screen.findByLabelText('User')
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
  serve({ 'GET /api/me': () => json(alice) })
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
  expect(screen.getByRole('heading').textContent).toBe('Order A/1')
  go('#/nowhere')
  expect(screen.getByRole('heading').textContent).toBe('Orders')
})
