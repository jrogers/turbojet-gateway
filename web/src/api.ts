import type { Changed, Me, Numbers, OrderDetail, OrdersPage, Page, RiskView, SessionView } from './types.ts'

/**
 * A refusal or failure: the status, and the API's `{"error": ...}` (`api::ApiError`), or the
 * status text when the body isn't that.
 */
export class ApiError extends Error {
  status: number

  constructor(status: number, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
  }
}

// The session's CSRF token, in memory only: a reload gets it again from `GET /api/me`.
let csrf: string | undefined
let ended: (reason?: string) => void = () => {}

/**
 * Registers what to do when the session ends: send the user to login. Answers a function that
 * unregisters it.
 */
export function onSessionEnded(handler: (reason?: string) => void): () => void {
  ended = handler
  return () => {
    if (ended === handler) ended = () => {}
  }
}

/**
 * The session has ended, or never was: forgets its token and calls the registered handler. A 401
 * calls it, and so does the live socket's refusal.
 */
export function sessionEnded(reason?: string) {
  csrf = undefined
  ended(reason)
}

/**
 * Sends a JSON request to the API and answers its JSON, or `undefined` for an empty answer
 * (204, 202). Every request but a GET carries the CSRF token once known. A 401 ends the session,
 * but for a login's: a wrong password ends nothing.
 */
export async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { Accept: 'application/json' }
  if (body !== undefined) headers['Content-Type'] = 'application/json'
  if (method !== 'GET' && csrf !== undefined) headers['X-CSRF-Token'] = csrf
  let response: Response
  let text: string
  try {
    response = await fetch(path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      credentials: 'same-origin',
    })
    text = await response.text()
  } catch {
    throw new ApiError(0, 'cannot reach the gateway')
  }
  if (response.ok) {
    if (text === '') return undefined as T
    try {
      return JSON.parse(text) as T
    } catch {
      throw new ApiError(response.status, 'unexpected answer from the API')
    }
  }
  if (response.status === 401 && !(method === 'POST' && path === '/api/login')) sessionEnded()
  const error = (lenient(text) as { error?: unknown } | undefined)?.error
  throw new ApiError(response.status, typeof error === 'string' ? error : response.statusText || `HTTP ${response.status}`)
}

/** An error answer's JSON, or `undefined`: a proxy's error page isn't. */
function lenient(text: string): unknown {
  try {
    return JSON.parse(text)
  } catch {
    return undefined
  }
}

function keep(me: Me): Me {
  csrf = me.csrf
  return me
}

/** A path segment: a session's id holds `>` and maybe `/`, which the API takes percent-encoded. */
const segment = encodeURIComponent

/** The query string of the parameters given, leaving out empty ones. */
function query(params: Record<string, string | number | boolean | null | undefined>): string {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && value !== '') search.append(key, String(value))
  }
  const text = search.toString()
  return text === '' ? '' : `?${text}`
}

export const me = () => request<Me>('GET', '/api/me').then(keep)
export const login = (user: string, password: string) => request<Me>('POST', '/api/login', { user, password }).then(keep)
/** Ends the session; on a failure the token is kept, so the user can try again. */
export async function logout() {
  await request<void>('POST', '/api/logout')
  csrf = undefined
}

export const sessions = () => request<SessionView[]>('GET', '/api/sessions')
export const risk = () => request<RiskView>('GET', '/api/risk')

/**
 * `GET /api/orders` with the filters given: `client`, `venue`, `symbol`, `status`, `working`,
 * `from`, `to`, `after` (a page's `next`, as it is) and `limit`.
 */
export const orders = (params: Record<string, string | number | boolean | null | undefined> = {}) =>
  request<OrdersPage>('GET', `/api/orders${query(params)}`)
export const order = (orderId: string) => request<OrderDetail>('GET', `/api/orders/${segment(orderId)}`)
export const orderMessages = (orderId: string, params: { limit?: number; after?: string | null } = {}) =>
  request<Page>('GET', `/api/orders/${segment(orderId)}/messages${query(params)}`)

// The actions: each takes the operator's reason, which the API requires and audits.
export const pause = (id: string, reason: string) => request<void>('POST', `/api/sessions/${segment(id)}/pause`, { reason })
export const resume = (id: string, reason: string) => request<void>('POST', `/api/sessions/${segment(id)}/resume`, { reason })
export const resend = (id: string, reason: string, from: number) =>
  request<Numbers>('POST', `/api/sessions/${segment(id)}/resend`, { reason, from })
export const cancelOrder = (orderId: string, reason: string) =>
  request<void>('POST', `/api/orders/${segment(orderId)}/cancel`, { reason })
export const requestStatus = (orderId: string, reason: string) =>
  request<void>('POST', `/api/orders/${segment(orderId)}/status`, { reason })
export const resolve = (orderId: string, reason: string) =>
  request<void>('POST', `/api/orders/${segment(orderId)}/resolve`, { reason })
// A scope is `all`, `comp:COMPID` or `session:SESSION`, as written.
export const block = (scope: string, reason: string) => request<Changed>('POST', '/api/risk/block', { reason, scope })
export const unblock = (scope: string, reason: string) => request<Changed>('POST', '/api/risk/unblock', { reason, scope })
/** The kill switch: blocks `scope` and cancels its orders; 204. */
export const cancelAll = (scope: string, reason: string) => request<void>('POST', '/api/risk/cancel-all', { reason, scope })
