import * as api from './api.ts'
import type { LiveMessage } from './types.ts'

/** What the console uses of a `WebSocket`. */
export type Socket = Pick<WebSocket, 'onopen' | 'onmessage' | 'onclose' | 'close'>

/** Opens a socket to `url`: `new WebSocket`, or a fake in tests. */
export type Opener = (url: string) => Socket

/** How the link stands: opening the first time, open, or lost and waiting to try again. */
export type Status = 'connecting' | 'open' | 'reconnecting'

// `api::live`'s close codes (RFC 6455): its auth recheck refusing is POLICY; AWAY (stopping, or no
// answer to pings) and AGAIN (too slow) are worth trying again, as is anything else.
export const POLICY = 1008

/** The first wait before reconnecting, and the longest, in ms. */
export const FIRST_WAIT = 1_000
export const MAX_WAIT = 30_000

/**
 * The wait before the `failures`th try in a row (1 for the first): doubling from [`FIRST_WAIT`]
 * to [`MAX_WAIT`], less up to half at random, so consoles that lost the gateway together don't
 * come back together.
 */
export function backoff(failures: number, random = Math.random): number {
  const wait = Math.min(MAX_WAIT, FIRST_WAIT * 2 ** (failures - 1))
  return wait - (wait / 2) * random()
}

/**
 * Opens `/api/live` on the page's host, handing each message to `onMessage`, and opens it again
 * after a close, waiting longer after each failure in a row. A socket that brings a message is a
 * good one, and starts the waits again from the first. A policy close is the session ending: it
 * goes to login and isn't tried again. Answers what closes it for good.
 */
export function connect({
  onMessage,
  onStatus,
  open = (url) => new WebSocket(url),
}: {
  onMessage: (message: LiveMessage) => void
  onStatus: (status: Status) => void
  open?: Opener
}): () => void {
  const url = `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/api/live`
  let socket: Socket | undefined
  let timer: ReturnType<typeof setTimeout> | undefined
  let failures = 0

  function start() {
    timer = undefined
    let opened = false
    const current = (socket = open(url))
    current.onopen = () => {
      opened = true
      onStatus('open')
    }
    current.onmessage = (event: MessageEvent) => {
      failures = 0
      onMessage(JSON.parse(event.data as string) as LiveMessage)
    }
    current.onclose = (event: CloseEvent) => {
      socket = undefined
      if (event.code === POLICY) return api.sessionEnded(event.reason || undefined)
      // A refused upgrade (a 401 among them) shows only as a close before opening: asking who we
      // are ends the session if that's why, rather than retrying it for ever.
      if (!opened) api.me().catch(() => {})
      failures += 1
      onStatus('reconnecting')
      timer = setTimeout(start, backoff(failures))
    }
  }

  start()
  return () => {
    clearTimeout(timer)
    if (socket) {
      socket.onclose = socket.onmessage = socket.onopen = null
      socket.close()
    }
  }
}
