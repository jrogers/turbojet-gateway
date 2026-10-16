import { afterEach, beforeEach, expect, test, vi } from 'vitest'

type Call = { url: string; init: RequestInit }

// A fresh module per test, so no token or handler carries over from another.
let api: typeof import('./api.ts')
beforeEach(async () => {
  vi.resetModules()
  api = await import('./api.ts')
})

// Fetches past the answers given: `request()` would hide the throw as unreachable, so they're
// checked after each test too.
let stray: string[] = []

/** Answers each fetch with the next of `answers`, recording the calls. */
function serve(...answers: Response[]): Call[] {
  const calls: Call[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit) => {
    calls.push({ url, init })
    const answer = answers.shift()
    if (answer === undefined) {
      stray.push(`${init.method} ${url}`)
      throw new Error(`unexpected fetch: ${init.method} ${url}`)
    }
    return answer
  })
  return calls
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })
const empty = () => new Response(null, { status: 204 })
const header = (call: Call, name: string) => (call.init.headers as Record<string, string>)[name]

afterEach(() => {
  vi.unstubAllGlobals()
  expect(stray).toEqual([])
  stray = []
})

test('sends the CSRF token on writes only, once known, until logged out', async () => {
  const me = { user: 'alice', role: 'operator', clients: null, csrf: 'tok' }
  const calls = serve(json({ error: 'log in' }, 401), json(me), json([]), empty(), empty(), empty())
  await expect(api.pause('x', 'why')).rejects.toThrow()
  expect(header(calls[0], 'X-CSRF-Token')).toBeUndefined()

  await api.me()
  await api.sessions()
  await api.pause('FIX.4.4:TURBOJETHUB->FUNDA/desk', 'maintenance')
  expect(header(calls[1], 'X-CSRF-Token')).toBeUndefined()
  expect(header(calls[2], 'X-CSRF-Token')).toBeUndefined()
  expect(header(calls[3], 'X-CSRF-Token')).toBe('tok')
  expect(calls[3].url).toBe('/api/sessions/FIX.4.4%3ATURBOJETHUB-%3EFUNDA%2Fdesk/pause')
  expect(calls[3].init).toMatchObject({ method: 'POST', credentials: 'same-origin', body: '{"reason":"maintenance"}' })

  await api.logout()
  expect(header(calls[4], 'X-CSRF-Token')).toBe('tok')
  await api.resume('x', 'why')
  expect(header(calls[5], 'X-CSRF-Token')).toBeUndefined()
})

test('a failed logout keeps the token, to try again', async () => {
  const calls = serve(json({ user: 'alice', role: 'admin', clients: null, csrf: 'tok' }), json({ error: 'boom' }, 500), empty())
  await api.me()
  await expect(api.logout()).rejects.toMatchObject({ status: 500, message: 'boom' })
  await api.logout()
  expect(header(calls[2], 'X-CSRF-Token')).toBe('tok')
})

test("maps an error to ApiError with the API's message", async () => {
  serve(json({ error: 'no order X1' }, 404), new Response('<html>bad gateway</html>', { status: 502, statusText: 'Bad Gateway' }))
  await expect(api.order('X1')).rejects.toMatchObject({ name: 'ApiError', status: 404, message: 'no order X1' })
  await expect(api.sessions()).rejects.toMatchObject({ status: 502, message: 'Bad Gateway' })
})

test("a 2xx that isn't JSON is an error", async () => {
  serve(new Response('<html>a captive portal</html>', { status: 200 }))
  await expect(api.sessions()).rejects.toMatchObject({ name: 'ApiError', status: 200, message: 'unexpected answer from the API' })
})

test("a fetch that fails is ApiError 0, the gateway unreachable", async () => {
  vi.stubGlobal('fetch', async () => {
    throw new TypeError('Failed to fetch')
  })
  await expect(api.sessions()).rejects.toMatchObject({ name: 'ApiError', status: 0, message: 'cannot reach the gateway' })
})

test("a 401 ends the session, but not a login's, nor another refusal", async () => {
  const ended = vi.fn()
  api.onSessionEnded(ended)
  serve(
    json({ error: 'wrong user or password' }, 401),
    json({ error: 'missing or wrong X-CSRF-Token' }, 403),
    json({ error: 'the session has ended: log in again' }, 401),
  )
  await expect(api.login('alice', 'guess')).rejects.toMatchObject({ status: 401, message: 'wrong user or password' })
  await expect(api.cancelOrder('X1', 'why')).rejects.toMatchObject({ status: 403 })
  expect(ended).not.toHaveBeenCalled()
  await expect(api.orders()).rejects.toMatchObject({ status: 401 })
  expect(ended).toHaveBeenCalledOnce()
})

test('an unregistered handler is no longer called', async () => {
  const ended = vi.fn()
  const unregister = api.onSessionEnded(ended)
  unregister()
  api.sessionEnded('closed')
  expect(ended).not.toHaveBeenCalled()
})

test('builds the orders query, leaving out empty filters', async () => {
  const calls = serve(json({ orders: [], next: null }))
  await api.orders({ client: 'comp:FUNDA', venue: '', working: true, after: 'c+1', from: undefined })
  expect(calls[0].url).toBe('/api/orders?client=comp%3AFUNDA&working=true&after=c%2B1')
})
