import { useState, type FormEvent } from 'react'
import { login } from './api.ts'
import type { Me } from './types.ts'

/** The login form: `POST /api/login`, then `onLogin` with who logged in. */
export default function Login({ onLogin, notice }: { onLogin: (me: Me) => void; notice?: string }) {
  const [user, setUser] = useState('')
  const [password, setPassword] = useState('')
  const [error, setError] = useState(notice)
  const [busy, setBusy] = useState(false)

  async function submit(event: FormEvent) {
    event.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      onLogin(await login(user, password))
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      setBusy(false)
    }
  }

  return (
    <main className="login">
      <form onSubmit={submit}>
        <h1>Jetbridge</h1>
        <label>
          User
          <input name="user" autoComplete="username" required value={user} onChange={(e) => setUser(e.target.value)} />
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
        {error && (
          <p className="error" role="alert">
            {error}
          </p>
        )}
        <button type="submit" disabled={busy}>
          Log in
        </button>
      </form>
    </main>
  )
}
