import { useState, type FormEvent } from 'react'
import { login, message } from './api.ts'
import type { Me } from './types.ts'

/**
 * The login form: `POST /api/login`, then `onLogin` with who logged in. Shows `notice` (why the
 * user is here) until a login fails.
 */
export default function Login({ onLogin, notice }: { onLogin: (me: Me) => void; notice?: string }) {
  const [user, setUser] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string>()
  const [busy, setBusy] = useState(false)

  async function submit(event: FormEvent) {
    event.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      onLogin(await login(user, password))
    } catch (e) {
      setError(message(e))
      setBusy(false)
    }
  }

  return (
    <main className="login">
      <div className="panel">
        <h1 className="brand">Turbojet Gateway</h1>
        <form onSubmit={submit}>
          <label>
            User
            <input name="user" autoFocus autoComplete="username" required value={user} onChange={(e) => setUser(e.target.value)} />
          </label>
          <label>
            Password
            <input
              name="password"
              type="password"
              autoComplete="current-password"
              required
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </label>
          {(error ?? notice) && (
            <p className="error" role="alert">
              {error ?? notice}
            </p>
          )}
          <button type="submit" className="primary" disabled={busy}>
            Log in
          </button>
        </form>
      </div>
    </main>
  )
}
