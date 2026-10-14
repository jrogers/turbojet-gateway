import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import * as api from './api.ts'
import { FakeSocket, fake } from './fake-socket.ts'
import { AGAIN, backoff, connect, FIRST_WAIT, MAX_WAIT, POLICY, type Status } from './live.ts'
import type { LiveMessage } from './types.ts'

let messages: LiveMessage[]
let statuses: Status[]
let stop: () => void

beforeEach(() => {
  vi.useFakeTimers()
  // No jitter: each wait is the longest it may be.
  vi.spyOn(Math, 'random').mockReturnValue(0)
  FakeSocket.opened = []
  messages = []
  statuses = []
  stop = connect({ onMessage: (m) => messages.push(m), onStatus: (s) => statuses.push(s), open: fake })
})

afterEach(() => {
  stop()
  vi.useRealTimers()
  vi.restoreAllMocks()
  vi.unstubAllGlobals()
})

const last = () => FakeSocket.opened[FakeSocket.opened.length - 1]

/** Waits `ms`, checking no socket opened until the last of it. */
function waitFor(ms: number) {
  const count = FakeSocket.opened.length
  vi.advanceTimersByTime(ms - 1)
  expect(FakeSocket.opened).toHaveLength(count)
  vi.advanceTimersByTime(1)
  expect(FakeSocket.opened).toHaveLength(count + 1)
}

test('opens /api/live on the page host, and hands on what it sends', () => {
  expect(last().url).toBe(`ws://${location.host}/api/live`)
  last().open()
  last().send({ type: 'blocked', scope: 'all' })
  expect(messages).toEqual([{ type: 'blocked', scope: 'all' }])
  expect(statuses).toEqual(['open'])
})

test('the wait doubles to the most, less up to half at random', () => {
  expect([1, 2, 3, 5, 6, 9].map((n) => backoff(n, () => 0))).toEqual([1_000, 2_000, 4_000, 16_000, MAX_WAIT, MAX_WAIT])
  expect(backoff(1, () => 0.999)).toBeGreaterThan(FIRST_WAIT / 2)
})

test('reconnects after a close, waiting longer each time, and from the first after a good socket', () => {
  // Opened but closed before a message, as AWAY and AGAIN do: each failure waits longer.
  last().open()
  last().end(1001, 'the gateway is stopping')
  expect(statuses).toEqual(['open', 'reconnecting'])
  waitFor(1_000)
  last().open()
  last().end(1013, 'too slow to keep up: reconnect')
  waitFor(2_000)
  last().open()
  last().end(1006)
  waitFor(4_000)

  last().open()
  last().send({ type: 'blocked', scope: 'all' })
  last().end(1001)
  waitFor(1_000)
})

test('a socket closed as too slow backs off, though it brought messages', () => {
  for (const wait of [1_000, 2_000, 4_000]) {
    last().open()
    last().send({ type: 'blocked', scope: 'all' })
    last().end(AGAIN, 'too slow to keep up: reconnect')
    waitFor(wait)
  }
})

test("a frame that isn't JSON is skipped", () => {
  last().open()
  last().onmessage?.call(last() as unknown as WebSocket, new MessageEvent('message', { data: '{not json' }))
  last().send({ type: 'unblocked', scope: 'all' })
  expect(messages).toEqual([{ type: 'unblocked', scope: 'all' }])
})

test('a close before opening, with the session still good, keeps trying, waiting longer', async () => {
  const fetch = vi.fn(async () => new Response(JSON.stringify({ user: 'u', role: 'viewer', clients: null }), { status: 200 }))
  vi.stubGlobal('fetch', fetch)
  for (const wait of [1_000, 2_000, 4_000]) {
    last().end(1006)
    waitFor(wait)
  }
  await vi.waitFor(() => expect(fetch).toHaveBeenCalledTimes(3))
})

test('a policy close ends the session, saying why, and stops', () => {
  const ended = vi.fn()
  const unregister = api.onSessionEnded(ended)
  last().open()
  last().end(POLICY, 'the session has ended: log in again')
  expect(ended).toHaveBeenCalledWith('the session has ended: log in again')
  vi.advanceTimersByTime(MAX_WAIT * 2)
  expect(FakeSocket.opened).toHaveLength(1)
  unregister()
})

test("a close before opening asks who we are: a 401 ends the session", async () => {
  const ended = vi.fn()
  const unregister = api.onSessionEnded(ended)
  const fetch = vi.fn(async () => new Response(JSON.stringify({ error: 'log in' }), { status: 401 }))
  vi.stubGlobal('fetch', fetch)
  last().end(1006)
  await vi.waitFor(() => expect(ended).toHaveBeenCalled())
  expect(fetch).toHaveBeenCalledWith('/api/me', expect.anything())
  unregister()
})

test('stopping closes the socket and any reconnect waiting', () => {
  const first = last()
  stop()
  expect(first.closed).toBe(true)

  stop = connect({ onMessage: () => {}, onStatus: () => {}, open: fake })
  last().open()
  last().end(1001)
  stop()
  vi.advanceTimersByTime(MAX_WAIT * 2)
  expect(FakeSocket.opened).toHaveLength(2)
})
