import { afterEach, expect, test, vi } from 'vitest'
import * as api from './api.ts'

type Call = { url: string; init: RequestInit }

/** Answers each fetch with the next of `answers`, recording the calls. */
function serve(...answers: Response[]): Call[] {
  const calls: Call[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit) => {
    calls.push({ url, init })
    return answers.shift()!
  })
  return calls
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })
const header = (call: Call, name: string) => (call.init.headers as Record<string, string>)[name]

afterEach(() => {
  vi.unstubAllGlobals()
  api.onUnauthorized(() => {})
})

test('sends the CSRF token on writes only, once known', async () => {
  const me = { user: 'alice', role: 'operator', clients: null, csrf: 'tok' }
  const calls = serve(json({ error: 'log in' }, 401), json(me), json([]), new Response(null, { status: 204 }), new Response(null, { status: 204 }))
  await expect(api.pause('x', 'why')).rejects.toThrow()
  expect(header(calls[0], 'X-CSRF-Token')).toBeUndefined()

  await api.me()
  await api.sessions()
  await api.pause('FIX.4.4:JETBRIDGE->FUNDA/desk', 'maintenance')
  expect(header(calls[1], 'X-CSRF-Token')).toBeUndefined()
  expect(header(calls[2], 'X-CSRF-Token')).toBeUndefined()
  expect(header(calls[3], 'X-CSRF-Token')).toBe('tok')
  expect(calls[3].url).toBe('/api/sessions/FIX.4.4%3AJETBRIDGE-%3EFUNDA%2Fdesk/pause')
  expect(calls[3].init).toMatchObject({ method: 'POST', credentials: 'same-origin', body: '{"reason":"maintenance"}' })

  await api.logout()
  expect(header(calls[4], 'X-CSRF-Token')).toBe('tok')
})

test("maps an error to ApiError with the API's message", async () => {
  serve(json({ error: 'no order X1' }, 404), new Response('<html>bad gateway</html>', { status: 502, statusText: 'Bad Gateway' }))
  await expect(api.order('X1')).rejects.toMatchObject({ name: 'ApiError', status: 404, message: 'no order X1' })
  await expect(api.sessions()).rejects.toMatchObject({ status: 502, message: 'Bad Gateway' })
})

test('calls onUnauthorized on a 401, and not on other refusals', async () => {
  const unauthorized = vi.fn()
  api.onUnauthorized(unauthorized)
  serve(json({ error: 'missing or wrong X-CSRF-Token' }, 403), json({ error: 'the session has ended: log in again' }, 401))
  await expect(api.cancelOrder('X1', 'why')).rejects.toMatchObject({ status: 403 })
  expect(unauthorized).not.toHaveBeenCalled()
  await expect(api.orders()).rejects.toMatchObject({ status: 401 })
  expect(unauthorized).toHaveBeenCalledOnce()
})

test('builds the orders query, leaving out empty filters', async () => {
  const calls = serve(json({ orders: [], next: null }))
  await api.orders({ client: 'comp:FUNDA', venue: '', working: true, after: 'c+1', from: undefined })
  expect(calls[0].url).toBe('/api/orders?client=comp%3AFUNDA&working=true&after=c%2B1')
})
