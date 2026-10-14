import { useEffect, useReducer, useRef, useState, type FormEvent } from 'react'
import * as api from './api.ts'
import { useLive } from './store.ts'
import type { Me, RouteTest, RouteView } from './types.ts'

const FIELDS = ['client', 'ex_destination', 'symbol', 'venue', 'reject'] as const

/** The same rule: every field alike. */
const sameRoute = (a: RouteView | null | undefined, b: RouteView | null | undefined) =>
  a != null && b != null && FIELDS.every((field) => a[field] === b[field])

/** What a dry run says when its rule isn't the table's: the rules were changed since read. */
export const CHANGED = 'The rules have changed since they were read; reading them again: test once more.'

/** What a dry run's answer says: the rule, 1-based as the table numbers them, and where it goes. */
function outcome({ rule, target }: RouteTest): string {
  if (rule === null || target === null) return 'No route matches: the order would be rejected "no route for this order".'
  return 'venue' in target
    ? `Rule ${rule + 1} matches: the order would go to ${target.venue}.`
    : `Rule ${rule + 1} matches: the order would be rejected "${target.reject}".`
}

/**
 * The routing rules in force, in the order they're tried, and a dry run: where an order of a
 * client, ExDestination and symbol would go, its rule highlighted. Routing only: blocks and risk
 * aren't checked. A scoped user tests only its clients; another's is refused with the API's text.
 * Read again on each snapshot and reload, which may change them; a dry run's answer goes then, as
 * its rule may be another's. A SIGHUP or another admin's reload changes them unseen: an answer whose
 * rule isn't the table's says so, and the rules are read again.
 */
export default function Routes({ me, reloads }: { me: Me; reloads: number }) {
  const live = useLive()
  // `undefined` until read; `null` after a 403 or 404, with `error` saying why.
  const [routes, setRoutes] = useState<RouteView[] | null>()
  const [error, setError] = useState<string>()
  const [tested, setTested] = useState<RouteTest>()
  const [testError, setTestError] = useState<string>()
  // The dry run asked last: an answer to an earlier one, or after unmounting, is dropped.
  const testing = useRef<object>(undefined)
  // The rules shown, for a dry run's answer to be checked against.
  const shown = useRef<RouteView[] | null>(undefined)
  // One more reads the rules again at once: after an answer naming a rule that has changed.
  const [reads, reread] = useReducer((n: number) => n + 1, 0)

  useEffect(() => {
    let current = true
    api.routes().then(
      (routes) => {
        if (!current) return
        shown.current = routes
        setRoutes(routes)
        setError(undefined)
        // A dry run asked of the rules before may name another rule now.
        testing.current = undefined
        setTested(undefined)
      },
      (e: unknown) => {
        if (!current) return
        if (e instanceof api.ApiError && (e.status === 403 || e.status === 404)) {
          shown.current = null
          setRoutes(null)
        }
        setError(api.message(e))
      },
    )
    return () => {
      current = false
    }
  }, [live.snapshots, reloads, reads])

  useEffect(
    () => () => {
      testing.current = undefined
    },
    [],
  )

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const form = new FormData(event.currentTarget)
    const text = (name: string) => String(form.get(name) ?? '').trim()
    const [client, exDestination, symbol] = [text('client'), text('ex_destination'), text('symbol')]
    if (client === '' || symbol === '') {
      setTested(undefined)
      setTestError('Client and symbol are required.')
      return
    }
    const at = {}
    testing.current = at
    api.routeTest(client, exDestination, symbol).then(
      (answer) => {
        if (testing.current !== at) return
        if (answer.rule !== null && !sameRoute(shown.current?.[answer.rule], answer.route)) {
          setTested(undefined)
          setTestError(CHANGED)
          reread()
          return
        }
        setTested(answer)
        setTestError(undefined)
      },
      (e: unknown) => {
        if (testing.current !== at) return
        setTested(undefined)
        setTestError(api.message(e))
      },
    )
  }

  return (
    <>
      <h1 tabIndex={-1}>Routes</h1>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      {routes === undefined ? (
        !error && <p className="muted">Loading…</p>
      ) : (
        routes !== null && <Rules routes={routes} matched={tested?.rule ?? null} />
      )}

      <section aria-labelledby="dry-run-title">
        <h2 id="dry-run-title">Dry run</h2>
        <form className="filters" onSubmit={submit}>
          <label>
            Client (CompID)
            <input name="client" list="route-clients" required />
          </label>
          {me.clients !== null && (
            <datalist id="route-clients">
              {me.clients.map((client) => (
                <option key={client} value={client} />
              ))}
            </datalist>
          )}
          <label>
            ExDestination
            <input name="ex_destination" />
          </label>
          <label>
            Symbol
            <input name="symbol" required />
          </label>
          <button type="submit" className="primary">
            Test
          </button>
        </form>
        {testError && (
          <p className="error" role="alert">
            {testError}
          </p>
        )}
        {/* Always there, so a screen reader hears its text change. */}
        <p role="status">{tested && outcome(tested)}</p>
      </section>
    </>
  )
}

/** The rules, numbered from 1, `matched` (an index) highlighted. */
function Rules({ routes, matched }: { routes: RouteView[]; matched: number | null }) {
  if (routes.length === 0) return <p className="muted">No routes: every order is rejected.</p>
  const any = <span className="muted">any</span>
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th className="num">#</th>
            <th>Client</th>
            <th>ExDestination</th>
            <th>Symbol</th>
            <th>Venue or reject</th>
          </tr>
        </thead>
        <tbody>
          {routes.map((route, i) => (
            <tr key={i} className={i === matched ? 'matched' : undefined} aria-current={i === matched ? 'true' : undefined}>
              <td className="num">{i + 1}</td>
              <td>{route.client ?? any}</td>
              <td>{route.ex_destination ?? any}</td>
              <td>{route.symbol ?? any}</td>
              <td>{route.venue ?? `reject "${route.reject ?? ''}"`}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
