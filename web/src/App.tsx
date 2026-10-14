import { useEffect, useMemo, useState } from 'react'
import Alarms, { AlarmCount, type Dismissed } from './Alarms.tsx'
import * as api from './api.ts'
import Login from './Login.tsx'
import Messages from './Messages.tsx'
import Order from './Order.tsx'
import Orders from './Orders.tsx'
import Risk from './Risk.tsx'
import Sessions from './Sessions.tsx'
import { LiveProvider, useLive } from './store.ts'
import { useRoute, type Route } from './router.ts'
import type { Me } from './types.ts'

const SESSION_ENDED = 'Your session has ended; log in again.'

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
        setNotice(unauthorized ? undefined : e instanceof Error ? e.message : String(e))
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
  return <Shell me={me} onLogout={() => enter(null)} />
}

/** The live state, for the whole shell: its header shows the alarm count, and the clients. */
function Shell(props: { me: Me; onLogout: () => void }) {
  return (
    <LiveProvider>
      <Frame {...props} />
    </LiveProvider>
  )
}

function Frame({ me: login, onLogout }: { me: Me; onLogout: () => void }) {
  const live = useLive()
  // The user's clients as the last snapshot said: a reload may have changed them since login.
  const me = useMemo(
    () => (live.clients === undefined ? login : { ...login, clients: live.clients }),
    [login, live.clients],
  )
  const route = useRoute()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const [dismissed, setDismissed] = useState<Dismissed>(() => new Set())

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
      setError(e instanceof Error ? e.message : String(e))
      setBusy(false)
    }
  }

  return (
    <>
      <header className="shell">
        <span className="brand">Jetbridge</span>
        <nav>
          <a href="#/orders" aria-current={current(route, 'orders')}>
            Orders
          </a>
          <a href="#/sessions" aria-current={current(route, 'sessions')}>
            Sessions
          </a>
          <a href="#/risk" aria-current={current(route, 'risk')}>
            Risk
          </a>
          <a href="#/messages" aria-current={current(route, 'messages')}>
            Messages
          </a>
        </nav>
        <AlarmCount dismissed={dismissed} />
        <span className="who">
          <span className="user">{me.user}</span> <span className="role">{me.role}</span>{' '}
          <span className="clients">{me.clients === null ? 'all clients' : me.clients.join(', ')}</span>
        </span>
        {error && (
          <span className="error" role="alert">
            {error}
          </span>
        )}
        <button type="button" onClick={logout} disabled={busy}>
          Log out
        </button>
      </header>
      <main>
        <Banner />
        <Alarms dismissed={dismissed} onDismiss={setDismissed} />
        <Screen route={route} me={me} />
      </main>
    </>
  )
}

/** A nav link's `aria-current`: an order's page is the blotter's. */
const current = (route: Route, screen: Exclude<Route['screen'], 'order'>) =>
  (route.screen === 'order' ? 'orders' : route.screen) === screen ? 'page' : undefined

/** Says so while the live socket is down: what the screens show is as it was. */
function Banner() {
  const { link } = useLive()
  if (link !== 'reconnecting') return null
  return (
    <p className="banner" role="status">
      Disconnected, reconnecting…
    </p>
  )
}

function Screen({ route, me }: { route: Route; me: Me }) {
  switch (route.screen) {
    case 'orders':
      return <Orders />
    case 'sessions':
      return <Sessions me={me} />
    case 'risk':
      return <Risk me={me} />
    case 'messages':
      return <Messages me={me} />
    case 'order':
      // Keyed: another order's page starts afresh.
      return <Order key={route.orderId} orderId={route.orderId} me={me} />
  }
}
