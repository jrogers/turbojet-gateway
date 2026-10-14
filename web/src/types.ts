// The API's views, by hand from the Rust structs' serde. Times are RFC 3339 strings (chrono's
// DateTime<Utc>). Amounts are strings: rust_decimal's `serde` feature writes a Decimal as a
// string, every digit kept, and the console never turns one into a float.

/** A decimal as the API writes it, `"100"`, `"10.25"`: `rust_decimal::Decimal`. */
export type Decimal = string

/** `auth::Role`, lowercase. */
export type Role = 'viewer' | 'operator' | 'admin'

/** `api::auth::Me`: `POST /api/login`'s and `GET /api/me`'s answer. */
export interface Me {
  user: string
  role: Role
  /** `null`: every client. */
  clients: string[] | null
  /** Only with a session (skipped when `None`): the `X-CSRF-Token` for every request but a GET. */
  csrf?: string
}

/** `api::read::SessionSide`. */
export type SessionSide = 'client' | 'venue'

/** `api::read::SessionState`. */
export type SessionState = 'disconnected' | 'connected' | 'logging_on' | 'logged_on' | 'logging_out'

/** `api::read::Activity`. */
export interface Activity {
  since: string
  last_received: string | null
  last_sent: string | null
  resending: boolean
}

/** `api::read::SessionView`: `GET /api/sessions`' items. */
export interface SessionView {
  side: SessionSide
  id: string
  /** A venue's name; `null` for a client's session. */
  venue: string | null
  status: SessionState
  next_incoming: number | null
  next_outgoing: number | null
  /** `null` while disconnected. */
  activity: Activity | null
  paused: boolean
}

/** `book::Side`. */
export type Side = 'buy' | 'sell' | 'sell_short'

/** `book::OrdType`. */
export type OrdType = 'market' | 'limit'

/** `book::Status`. */
export type Status =
  | 'pending_new'
  | 'new'
  | 'partially_filled'
  | 'filled'
  | 'pending_cancel'
  | 'pending_replace'
  | 'canceled'
  | 'expired'
  | 'rejected'

/** `book::view::CancelState`. */
export type CancelState = 'waiting' | 'in_flight' | 'stuck'

/** `book::view::GatewayCancelView`. */
export interface GatewayCancelView {
  state: CancelState
  tries: number
}

/** `book::view::OrderView`: an order as a blotter shows it. */
export interface OrderView {
  /** The book's key and `GET /api/orders`' paging cursor; `order_id` is what the wire knows. */
  id: number
  order_id: string
  /** The client's session (`book::ClientId`, as it shows). */
  client: string
  cl_ord_id: string
  venue: string
  symbol: string
  side: Side
  ord_type: OrdType
  qty: Decimal
  /** `null` for a Market order. */
  price: Decimal | null
  cum_qty: Decimal
  avg_px: Decimal
  status: Status
  pending_cancel: boolean
  pending_replace: boolean
  gateway_cancel: GatewayCancelView | null
  unknown_at_venue: boolean
  created: string
}

/**
 * `api::read::OrdersPage`. `next` is `api::read::Next`, untagged: a number for the book's
 * orders, an opaque string for past ones (`from`); `null` after a short page.
 */
export interface OrdersPage {
  orders: OrderView[]
  next: number | string | null
}

/** `book::view::RequestKind`. */
export type RequestKind = 'new' | 'cancel' | 'replace'

/** `book::Origin`. */
export type Origin = 'client' | 'gateway'

/** `book::view::RequestView`: a request sent to the order's venue. */
export interface RequestView {
  downstream: string
  kind: RequestKind
  origin: Origin
  /** `null` for the gateway's own. */
  cl_ord_id: string | null
  sent_at: string
  answered: boolean
}

/** `book::view::OrderDetail`: `GET /api/orders/{order_id}`, the order flattened in. */
export interface OrderDetail extends OrderView {
  venue_order_id: string | null
  requests: RequestView[]
}

/** `api::messages::Hit`: a message found in the log. */
export interface Hit {
  time: string
  direction: 'in' | 'out'
  /** `null` for a client's message before its Logon. */
  session: string | null
  /** `null`, with no fields, for a frame that doesn't decode. */
  msg_type: string | null
  /** `[tag, value]` pairs, as on the wire. */
  fields: [number, string][]
  /** As on the wire, `|` for SOH. */
  raw: string
}

/** `api::messages::Page`: the message log's answer, paged by `next` (pass it as `after`). */
export interface Page {
  messages: Hit[]
  next: string | null
  follow: string | null
}

/** `book::view::AlarmKind`. */
export type AlarmKind =
  | 'gateway_unavailable'
  | 'gateway_available'
  | 'stuck_cancel'
  | 'orphan'
  | 'overfill'
  | 'unknown_order'
  | 'unknown_at_venue'
  | 'fill_on_done_order'
  | 'venue_store_reset'

/** `book::view::Alarm`. */
export interface Alarm {
  time: string
  kind: AlarmKind
  /** The session of the order the alarm is about; `null` for one about no order. */
  client: string | null
  text: string
}

/** `api::write::Numbers`: what the sequence-number actions answer. */
export interface Numbers {
  next_incoming: number
  next_outgoing: number
}

/** `risk::Limits`: one CompID's limits, every key written, `null` (or `false`) for no check. */
export interface Limits {
  max_order_qty: Decimal | null
  max_notional: Decimal | null
  symbols: string[] | null
  /** A percentage either side of the symbol's reference price. */
  price_band: Decimal | null
  credit: Decimal | null
  self_trade: boolean
}

/** `book::view::SessionRisk`: a session's open notional and what it has filled in its day. */
export interface SessionRisk {
  /** The session (`book::ClientId`, as it shows). */
  client: string
  open: Decimal
  filled: Decimal
}

/**
 * `book::view::RiskView`: `GET /api/risk`. A scoped user's clients' only (`RiskView::retain_for`):
 * the defaults and reference prices are everyone's.
 */
export interface RiskView {
  defaults: Limits
  /** Each with the defaults' keys it doesn't set filled in: the limits as merged. */
  by_comp_id: Record<string, Limits>
  sessions: SessionRisk[]
  /** Each symbol's last fill price. */
  references: Record<string, Decimal>
  /** Scopes as written, in the order blocked. */
  blocked: string[]
}

/** `api::write::Changed`: what a block or unblock answers. */
export interface Changed {
  changed: boolean
}

/** `api::live::Snapshot`: the socket's first message, and again after it falls behind. */
export interface Snapshot {
  type: 'snapshot'
  sessions: SessionView[]
  /** The working orders, a page at most. */
  orders: OrderView[]
  /** With a full page, the `after` for `GET /api/orders?working=true`. */
  orders_next: number | null
  /** Scopes as written: `all`, `comp:COMPID`, `session:SESSION` (`book::Scope`). */
  blocked: string[]
  unavailable: boolean
  /** The user's clients the snapshot was taken under: a reload may change them. `null`, all. */
  clients: string[] | null
}

/** `api::live::SessionChange`'s `event`. */
export type SessionEvent =
  | 'connected'
  | 'logged_on'
  | 'logging_out'
  | 'resend_started'
  | 'resend_finished'
  | 'disconnected'
  | 'paused'
  | 'resumed'
  | 'refused'

/** `api::live::SessionChange`. */
export interface SessionChange {
  type: 'session'
  side: SessionSide
  id: string
  event: SessionEvent
  /** Only for `refused` (skipped when `None`). */
  reason?: string
}

/**
 * What `/api/live` sends: the snapshot, `book::view::GatewayEvent` (tagged by `type`) and
 * `SessionChange`.
 */
export type LiveMessage =
  | Snapshot
  | ({ type: 'order' } & OrderView)
  | { type: 'blocked'; scope: string }
  | { type: 'unblocked'; scope: string }
  | ({ type: 'alarm' } & Alarm)
  | SessionChange
