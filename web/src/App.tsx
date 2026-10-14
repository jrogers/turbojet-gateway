import { useEffect, useState } from 'react'
import * as api from './api.ts'
import Login from './Login.tsx'
import { useRoute, type Route } from './router.ts'
import type { Me } from './types.ts'

/**
 * The console: login until `GET /api/me` or a login says who the user is, then the shell. A 401
 * from any request returns to login.
 */
export default function App() {
  // `undefined` while `GET /api/me` is asked; `null` logged out.
  const [me, setMe] = useState<Me | null>()
  const [notice, setNotice] = useState<string>()

  useEffect(() => {
    api.onUnauthorized(() => setMe(null))
    api.me().then(setMe, (e: unknown) => {
      setMe(null)
      // A 401 just means logging in; anything else is worth saying.
      if (!(e instanceof api.ApiError && e.status === 401)) setNotice(e instanceof Error ? e.message : String(e))
    })
  }, [])

  if (me === undefined) return null
  if (me === null) return <Login onLogin={setMe} notice={notice} />
  return <Shell me={me} onLogout={() => setMe(null)} />
}

function Shell({ me, onLogout }: { me: Me; onLogout: () => void }) {
  const route = useRoute()
  // Back to login whatever the answer: a session that's already ended is logged out too.
  const logout = () => api.logout().catch(() => {}).then(onLogout)
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
        <button type="button" onClick={logout}>
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
