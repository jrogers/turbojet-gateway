import { useEffect, useState } from 'react'
import * as api from './api.ts'
import Login from './Login.tsx'
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

function Shell({ me, onLogout }: { me: Me; onLogout: () => void }) {
  const route = useRoute()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

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
          <a href="#/orders" aria-current={route.screen === 'sessions' ? undefined : 'page'}>
            Orders
          </a>
          <a href="#/sessions" aria-current={route.screen === 'sessions' ? 'page' : undefined}>
            Sessions
          </a>
        </nav>
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
        <Screen route={route} />
      </main>
    </>
  )
}

// Placeholders until the screens are written (tasks 3 to 5).
function Screen({ route }: { route: Route }) {
  switch (route.screen) {
    case 'orders':
      return <h1>Orders</h1>
    case 'sessions':
      return <h1>Sessions</h1>
    case 'order':
      return <h1>Order {route.orderId}</h1>
  }
}
