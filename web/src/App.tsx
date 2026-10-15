import { useEffect, useMemo, useReducer, useRef, useState } from 'react'
import Alarms, { AlarmCount, type Dismissed } from './Alarms.tsx'
import * as api from './api.ts'
import Audit from './Audit.tsx'
import { Reloaded } from './Limits.tsx'
import Login from './Login.tsx'
import Messages from './Messages.tsx'
import Order from './Order.tsx'
import Orders from './Orders.tsx'
import Reason from './Reason.tsx'
import Risk from './Risk.tsx'
import Routes from './Routes.tsx'
import Sessions from './Sessions.tsx'
import { isDown } from './sessions.ts'
import { LiveProvider, useLive } from './store.ts'
import { useRoute, type Route } from './router.ts'
import type { Me, ReloadView } from './types.ts'

const SESSION_ENDED = 'Your session has ended; log in again.'

/** An admin without clients: the audit log and a reload reach every client, so they're its only. */
const unscopedAdmin = (me: Me) => me.role === 'admin' && me.clients === null

/** What the last reload said: what it took, or, for a 422, why a file was kept. */
type Reload = { took: ReloadView } | { kept: string }

/**
 * The console: login until `GET /api/me` or a login says who the user is, then the shell. A 401
 * from any request returns to login.
 */
export default function App() {
  // `undefined` while `GET /api/me` is asked; `null` logged out.
  const [me, setMe] = useState<Me | null>()
  const [notice, setNotice] = useState<string>()

  useEffect(() => {
    // StrictMode runs this twice: the first run's answer mustn't land after a login.
    let live = true
    const unregister = api.onSessionEnded((reason) => {
      if (!live) return
      setMe(null)
      setNotice(reason ?? SESSION_ENDED)
    })
    api.me().then(
      (me) => live && setMe(me),
      (e: unknown) => {
        if (!live) return
        setMe(null)
        // A 401 here is no session yet, not one ended: just log in. Anything else is worth saying.
        const unauthorized = e instanceof api.ApiError && e.status === 401
        setNotice(unauthorized ? undefined : api.message(e))
      },
    )
    return () => {
      live = false
      unregister()
    }
  }, [])

  const enter = (me: Me | null) => {
    setNotice(undefined)
    setMe(me)
  }
  if (me === undefined) return null
  if (me === null) return <Login onLogin={enter} notice={notice} />
  return <Shell me={me} onMe={setMe} onLogout={() => enter(null)} />
}

/** The live state, for the whole shell: its nav shows counts and the clients, its header the alarms. */
function Shell(props: { me: Me; onMe: (me: Me) => void; onLogout: () => void }) {
  return (
    <LiveProvider>
      <Frame {...props} />
    </LiveProvider>
  )
}

function Frame({ me: login, onMe, onLogout }: { me: Me; onMe: (me: Me) => void; onLogout: () => void }) {
  const live = useLive()
  // A snapshot after the first may follow a reload or a reconnection, either of which may come
  // with a new role: ask again who the user is. A failure keeps what was known; a 401 logs out.
  useEffect(() => {
    if (live.snapshots < 2) return
    let current = true
    api.me().then(
      (me) => current && onMe(me),
      () => {},
    )
    return () => {
      current = false
    }
  }, [live.snapshots, onMe])
  // The user's clients as the last snapshot said, over `GET /api/me`'s: they come with the
  // snapshot, so the screens rescope with what it shows, without waiting on that answer.
  const me = useMemo(
    () => (live.clients === undefined ? login : { ...login, clients: live.clients }),
    [login, live.clients],
  )
  const route = useRoute()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const [dismissed, setDismissed] = useState<Dismissed>(() => new Set())
  const [reloading, setReloading] = useState(false)
  // Counts the reloads done, so the screens showing what one changes read again.
  const [reloads, countReload] = useReducer((n: number) => n + 1, 0)
  const admin = unscopedAdmin(me)

  // Back to login once logged out, or on a 401: the session had ended already. Any other failure
  // keeps the session, and says so.
  async function logout() {
    setBusy(true)
    setError(undefined)
    try {
      await api.logout()
      onLogout()
    } catch (e) {
      if (e instanceof api.ApiError && e.status === 401) return onLogout()
      setError(api.message(e))
      setBusy(false)
    }
  }

  // The nav's counts. The working orders: at least those, when the snapshot had only a page of
  // them. The venues down, as `isDown` says.
  const working = live.orders.size
  const more = live.ordersNext !== null
  const down = live.sessions.filter((session) => session.side === 'venue' && isDown(session)).length

  return (
    <div className="app">
      <div className="sidebar">
        <span className="brand">Jetbridge</span>
        <nav className="links" aria-label="Main">
          <a href="#/orders" aria-current={current(route, 'orders')}>
            Orders
            {working > 0 && (
              <Count
                shown={more ? `${working}+` : working}
                said={more ? `more than ${working} working` : `${working} working`}
              />
            )}
          </a>
          <a href="#/sessions" aria-current={current(route, 'sessions')}>
            Sessions
            {down > 0 && <Count shown={`${down} down`} said={`${down} down`} warn />}
          </a>
          <a href="#/risk" aria-current={current(route, 'risk')}>
            Risk
          </a>
          <a href="#/messages" aria-current={current(route, 'messages')}>
            Messages
          </a>
          <a href="#/routes" aria-current={current(route, 'routes')}>
            Routes
          </a>
          {admin && (
            <a href="#/audit" aria-current={current(route, 'audit')}>
              Audit
            </a>
          )}
        </nav>
        <div className="who">
          <span>
            <span className="user">{me.user}</span> <span className="role">{me.role}</span>
          </span>
          <span className="clients">{me.clients === null ? 'all clients' : me.clients.join(', ')}</span>
          {error && (
            <span className="error" role="alert">
              {error}
            </span>
          )}
          <div className="buttons">
            {admin && (
              <button type="button" onClick={() => setReloading(true)}>
                Reload…
              </button>
            )}
            <button type="button" onClick={logout} disabled={busy}>
              Log out
            </button>
          </div>
        </div>
      </div>
      <main>
        <header className="page">
          <LinkState />
          <AlarmCount dismissed={dismissed} />
        </header>
        <Alarms dismissed={dismissed} onDismiss={setDismissed} />
        {admin && <ReloadStatus asking={reloading} onClose={() => setReloading(false)} onReloaded={countReload} />}
        <Screen route={route} me={me} reloads={reloads} />
      </main>
    </div>
  )
}

/** A nav link's count: `shown` beside its text, `said` to a screen reader, as "Orders, 2 working". */
function Count({ shown, said, warn }: { shown: number | string; said: string; warn?: boolean }) {
  return (
    <>
      <span className={warn ? 'count pill warn' : 'count'} aria-hidden="true">
        {shown}
      </span>
      <span className="visually-hidden">, {said}</span>
    </>
  )
}

/** A nav link's `aria-current`: an order's page is the blotter's. */
const current = (route: Route, screen: Exclude<Route['screen'], 'order'>) =>
  (route.screen === 'order' ? 'orders' : route.screen) === screen ? 'page' : undefined

/**
 * How the live socket stands: live, still connecting, or down, so what the screens show is as it
 * was. Always there, so a screen reader hears it go down, and come back.
 */
function LinkState() {
  const { link } = useLive()
  return (
    <span role="status">
      {link === 'open' ? (
        <span className="pill ok">
          <span className="dot" />
          Live
        </span>
      ) : link === 'connecting' ? (
        <span className="pill muted">Connecting…</span>
      ) : (
        <span className="pill warn">Disconnected, reconnecting…</span>
      )}
    </span>
  )
}

/** The screen the route names; one the user may not see says so, asking nothing of the API. */
function Screen({ route, me, reloads }: { route: Route; me: Me; reloads: number }) {
  switch (route.screen) {
    case 'orders':
      return <Orders />
    case 'sessions':
      return <Sessions me={me} />
    case 'risk':
      return <Risk me={me} reloads={reloads} />
    case 'messages':
      return <Messages me={me} />
    case 'routes':
      return <Routes me={me} reloads={reloads} />
    case 'audit':
      return unscopedAdmin(me) ? <Audit /> : <Hidden title="Audit" />
    case 'order':
      // Keyed: another order's page starts afresh.
      return <Order key={route.orderId} orderId={route.orderId} me={me} />
  }
}

function Hidden({ title }: { title: string }) {
  return (
    <>
      <h1 tabIndex={-1}>{title}</h1>
      <p className="muted">Not available to you.</p>
    </>
  )
}

/**
 * A reload's outcome, under the header on every screen, and while `asking`, its reason dialog.
 * What it took and its warnings, or for a 422, why a file was kept: the other applied, so that's
 * an outcome, not a failure to retry; any other failure stays in the dialog. Focus goes to the
 * screen's heading once done.
 */
function ReloadStatus({ asking, onClose, onReloaded }: { asking: boolean; onClose: () => void; onReloaded: () => void }) {
  const [reloaded, setReloaded] = useState<Reload>()
  // Set by a reload done: focus goes to the heading once its dialog has closed.
  const refocus = useRef(false)

  useEffect(() => {
    if (!asking && refocus.current) {
      refocus.current = false
      document.querySelector<HTMLElement>('main h1')?.focus()
    }
  }, [asking])

  async function reload(reason: string) {
    try {
      setReloaded({ took: await api.reload(reason) })
    } catch (e) {
      if (!(e instanceof api.ApiError && e.status === 422)) throw e
      setReloaded({ kept: e.message })
    }
    onReloaded()
    refocus.current = true
  }

  return (
    <div className="reloaded">
      {/* Always there, so a screen reader hears its text change. */}
      <div role="status">
        {reloaded &&
          ('took' in reloaded ? (
            <Reloaded reload={reloaded.took} />
          ) : (
            <p className="error">The reload kept a file: {reloaded.kept}</p>
          ))}
      </div>
      {reloaded && (
        <button type="button" onClick={() => setReloaded(undefined)}>
          Dismiss
        </button>
      )}
      {asking && (
        <Reason title="Reload the configuration" confirmLabel="Reload" onConfirm={reload} onClose={onClose}>
          <p>Rereads the configuration files, as SIGHUP does, and says what changed.</p>
        </Reason>
      )}
    </div>
  )
}
