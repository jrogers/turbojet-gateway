import { useEffect, useReducer, useRef, useState } from 'react'
import * as api from './api.ts'
import RiskLimits from './Limits.tsx'
import Reason from './Reason.tsx'
import { namesTarget, targetCompId } from './sessions.ts'
import { useLive } from './store.ts'
import type { Limits, Me, RiskView } from './types.ts'

/** How often the risk is read again, in ms: no live event carries notionals. */
export const RISK_EVERY = 5_000

type Action = { kind: 'block' | 'unblock' | 'cancel-all'; scope: string }

const message = (e: unknown) => (e instanceof Error ? e.message : String(e))

/**
 * Risk: each session's open and filled notional beside its limits, the reference prices, and the
 * blocks, live from the store. An operator or admin may block a scope, lift a block, or pull the
 * kill switch (block and cancel all), each with a reason; a scoped one only its clients', as the
 * API allows. Below, the limits as written, which an admin edits (`Limits.tsx`).
 */
export default function Risk({ me, reloads }: { me: Me; reloads: number }) {
  const live = useLive()
  // `undefined` until read; `null` after a 403 or 404, with `error` saying why.
  const [risk, setRisk] = useState<RiskView | null>()
  const [error, setError] = useState<string>()
  const [scope, setScope] = useState('')
  const [typed, setTyped] = useState('')
  const [action, setAction] = useState<Action>()
  const [done, setDone] = useState<string>()
  const heading = useRef<HTMLHeadingElement>(null)
  // Set by an action done: focus goes to the heading once its dialog has closed.
  const refocus = useRef(false)
  // One more reads again at once: after an action, or a limit edit.
  const [reads, reread] = useReducer((n: number) => n + 1, 0)

  // At once, then every RISK_EVERY while the page is seen and on its becoming seen, and again on
  // each snapshot, console reload or `reread`. A read answering after a newer one was asked, or
  // after unmounting, is dropped. A 403 or 404 stops the reads until the next snapshot or reload:
  // the scope must change first.
  const asking = useRef<object>(undefined)
  useEffect(() => {
    let stopped = false
    const read = () => {
      if (document.hidden || stopped) return
      const at = {}
      asking.current = at
      api.risk().then(
        (risk) => {
          if (asking.current !== at) return
          setRisk(risk)
          setError(undefined)
        },
        (e: unknown) => {
          if (asking.current !== at) return
          if (e instanceof api.ApiError && (e.status === 403 || e.status === 404)) {
            stopped = true
            setRisk(null)
          }
          setError(message(e))
        },
      )
    }
    read()
    const timer = setInterval(read, RISK_EVERY)
    document.addEventListener('visibilitychange', read)
    return () => {
      clearInterval(timer)
      document.removeEventListener('visibilitychange', read)
      asking.current = undefined
    }
  }, [live.snapshots, reloads, reads])

  useEffect(() => {
    if (action === undefined && refocus.current) {
      refocus.current = false
      heading.current?.focus()
    }
  }, [action])

  const acts = me.role !== 'viewer'
  const choose = (kind: Action['kind'], target: string) => {
    setDone(undefined)
    setTyped('')
    setAction({ kind, scope: target })
  }

  async function confirm(reason: string) {
    if (!action) return
    const { kind, scope } = action
    if (kind === 'cancel-all') {
      await api.cancelAll(scope, reason)
      setDone(`Blocked ${scope} and sent cancels for its working orders; each Canceled shows as its venue confirms it.`)
    } else if (kind === 'block') {
      const { changed } = await api.block(scope, reason)
      setDone(changed ? `Blocked ${scope}.` : `${scope} was blocked already.`)
    } else {
      const { changed } = await api.unblock(scope, reason)
      setDone(changed ? `Unblocked ${scope}.` : `${scope} wasn't blocked.`)
    }
    refocus.current = true
    reread()
  }

  // The scopes the form offers; the API refuses a scoped user any other. `me`'s clients are the
  // last snapshot's, as the shell merges them.
  const clients = me.clients
  const sessions = [
    ...new Set([...live.sessions.filter((s) => s.side === 'client').map((s) => s.id), ...(risk?.sessions ?? []).map((s) => s.client)]),
  ]
  const compIds = clients ?? [...new Set([...Object.keys(risk?.by_comp_id ?? {}), ...sessions.map(targetCompId)])]
  const offered = [
    ...(clients === null ? ['all'] : []),
    ...compIds.map((c) => `comp:${c}`),
    ...sessions.map((s) => `session:${s}`),
  ]

  const limitsOf = (session: string): Limits => {
    const compId = Object.keys(risk!.by_comp_id).find((c) => namesTarget(session, c))
    return compId === undefined ? risk!.defaults : risk!.by_comp_id[compId]
  }

  return (
    <>
      <h1 ref={heading} tabIndex={-1}>
        Risk
      </h1>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      {/* Always there, so a screen reader hears its text change. */}
      <p role="status">{done}</p>

      <section>
        <h2>Blocks</h2>
        {!live.ready ? (
          <p className="muted">Loading…</p>
        ) : live.blocked.length === 0 ? (
          <p className="muted">None.</p>
        ) : (
          <ul className="blocks">
            {live.blocked.map((blocked) => (
              <li key={blocked}>
                <code>{blocked}</code>
                {/* `all` is no scoped user's to lift, as the API says. */}
                {acts && (clients === null || blocked !== 'all') && (
                  <button type="button" onClick={() => choose('unblock', blocked)}>
                    Unblock
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
        {acts && (
          <form
            className="filters"
            onSubmit={(e) => {
              e.preventDefault()
              if (scope.trim() !== '') choose('block', scope.trim())
            }}
          >
            <label>
              Scope
              <input
                list="risk-scopes"
                placeholder="all, comp:COMPID or session:SESSION"
                value={scope}
                onChange={(e) => setScope(e.target.value)}
              />
            </label>
            <datalist id="risk-scopes">
              {offered.map((name) => (
                <option key={name} value={name} />
              ))}
            </datalist>
            <button type="submit" disabled={scope.trim() === ''}>
              Block
            </button>
            <button
              type="button"
              className="danger"
              disabled={scope.trim() === ''}
              onClick={() => choose('cancel-all', scope.trim())}
            >
              Cancel all orders…
            </button>
          </form>
        )}
      </section>

      {risk === undefined ? (
        !error && <p className="muted">Loading…</p>
      ) : (
        risk !== null && (
          <>
            <section>
              <h2>Exposure</h2>
              <Exposure risk={risk} limitsOf={limitsOf} />
            </section>
            <section>
              <h2>Reference prices</h2>
              <References references={risk.references} />
            </section>
          </>
        )
      )}

      <section className="limits" aria-labelledby="limits-title">
        <h2 id="limits-title">Limits</h2>
        <RiskLimits
          me={me}
          reloads={reloads}
          heading={heading}
          onDone={(text) => {
            setDone(text)
            reread()
          }}
        />
      </section>

      {action && (
        <Reason
          title={
            action.kind === 'cancel-all'
              ? `Cancel all orders of ${action.scope}`
              : `${action.kind === 'block' ? 'Block' : 'Unblock'} ${action.scope}`
          }
          onConfirm={confirm}
          onClose={() => setAction(undefined)}
          ready={action.kind !== 'cancel-all' || typed === action.scope}
          confirmLabel={action.kind === 'cancel-all' ? 'Cancel all orders' : undefined}
        >
          {action.kind === 'cancel-all' && (
            <>
              <p>Blocks {action.scope} and cancels its clients’ working orders at their venues.</p>
              <label>
                Type {action.scope} to confirm
                <input
                  autoComplete="off"
                  spellCheck={false}
                  autoCapitalize="off"
                  value={typed}
                  onChange={(e) => setTyped(e.target.value)}
                />
              </label>
            </>
          )}
        </Reason>
      )}
    </>
  )
}

/** Each session's notionals beside its limits as merged. Decimals as the API wrote them. */
function Exposure({ risk, limitsOf }: { risk: RiskView; limitsOf: (session: string) => Limits }) {
  if (risk.sessions.length === 0) return <p className="muted">None.</p>
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Session</th>
            <th className="num">Open</th>
            <th className="num">Filled</th>
            <th className="num">Credit</th>
            <th className="num">Max notional</th>
            <th className="num">Max order qty</th>
            <th className="num">Price band %</th>
            <th>Symbols</th>
            <th>Self trade</th>
          </tr>
        </thead>
        <tbody>
          {risk.sessions.map((session) => {
            const limits = limitsOf(session.client)
            return (
              <tr key={session.client}>
                <td>{session.client}</td>
                <td className="num">{session.open}</td>
                <td className="num">{session.filled}</td>
                <td className="num">{limits.credit ?? ''}</td>
                <td className="num">{limits.max_notional ?? ''}</td>
                <td className="num">{limits.max_order_qty ?? ''}</td>
                <td className="num">{limits.price_band ?? ''}</td>
                <td>{limits.symbols?.join(', ') ?? ''}</td>
                <td>{limits.self_trade ? 'refused' : ''}</td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

function References({ references }: { references: RiskView['references'] }) {
  const symbols = Object.entries(references)
  if (symbols.length === 0) return <p className="muted">None.</p>
  return (
    <div className="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Symbol</th>
            <th className="num">Last fill</th>
          </tr>
        </thead>
        <tbody>
          {symbols.map(([symbol, price]) => (
            <tr key={symbol}>
              <td>{symbol}</td>
              <td className="num">{price}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
