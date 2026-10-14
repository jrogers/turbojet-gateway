import { useEffect, useState } from 'react'
import * as api from './api.ts'
import Reason from './Reason.tsx'
import { useLive } from './store.ts'
import type { Me, SessionView } from './types.ts'

/** How often the sequence numbers are read again, in ms: the live events don't carry them. */
export const NUMBERS_EVERY = 5_000

type Numbers = Pick<SessionView, 'next_incoming' | 'next_outgoing'>

const key = (session: Pick<SessionView, 'side' | 'id'>) => `${session.side} ${session.id}`

type Action = { kind: 'pause' | 'resume' | 'resend'; session: SessionView }

/**
 * The client and venue sessions, live: state, next sequence numbers, paused. An operator or admin
 * may pause, resume or ask for a resend, each with a reason; a scoped one only its clients', as
 * the API allows.
 */
export default function Sessions({ me }: { me: Me }) {
  const live = useLive()
  const [action, setAction] = useState<Action>()
  const [from, setFrom] = useState('')
  const [done, setDone] = useState<string>()
  // The latest sequence numbers read, by `key`, over the store's: those change with every message,
  // and only a snapshot says them.
  const [numbers, setNumbers] = useState<Map<string, Numbers>>(new Map())

  useEffect(() => {
    // At once, so they're fresh on opening, then every NUMBERS_EVERY while the page is seen.
    const read = () => {
      if (document.hidden) return
      api.sessions().then(
        (sessions) =>
          setNumbers(new Map(sessions.map((s) => [key(s), { next_incoming: s.next_incoming, next_outgoing: s.next_outgoing }]))),
        // A 401 has ended the session already; anything else, the next read may do better.
        () => {},
      )
    }
    read()
    const timer = setInterval(read, NUMBERS_EVERY)
    return () => clearInterval(timer)
  }, [])

  const acts = (session: SessionView) =>
    me.role !== 'viewer' && (session.side === 'client' || me.clients === null)

  const choose = (kind: Action['kind'], session: SessionView) => {
    setDone(undefined)
    setFrom('')
    setAction({ kind, session })
  }

  async function confirm(reason: string) {
    if (!action) return
    const { kind, session } = action
    const name = session.venue ?? session.id
    if (kind === 'resend') {
      const seq = Number(from)
      if (!Number.isInteger(seq) || seq < 1) throw new Error('From must be a sequence number, 1 or more.')
      const answer = await api.resend(session.id, reason, seq)
      setNumbers((numbers) => new Map(numbers).set(key(session), answer))
      setDone(`Asked ${name} to resend from ${seq}; next incoming ${answer.next_incoming}.`)
    } else {
      await api[kind](session.id, reason)
      setDone(`${kind === 'pause' ? 'Paused' : 'Resumed'} ${name}.`)
    }
  }

  const table = (side: SessionView['side'], title: string) => {
    const sessions = live.sessions.filter((session) => session.side === side)
    return (
      <section>
        <h2>{title}</h2>
        {sessions.length === 0 ? (
          <p className="muted">None.</p>
        ) : (
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>Session</th>
                  <th>State</th>
                  <th className="num">Next in</th>
                  <th className="num">Next out</th>
                  <th>Paused</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {sessions.map((row) => ({ ...row, ...numbers.get(key(row)) })).map((session) => (
                  <tr key={session.id}>
                    <td title={session.id}>{session.venue ?? session.id}</td>
                    <td>
                      {session.status.replaceAll('_', ' ')}
                      {session.activity?.resending && ', resending'}
                    </td>
                    <td className="num">{session.next_incoming ?? ''}</td>
                    <td className="num">{session.next_outgoing ?? ''}</td>
                    <td>{session.paused ? 'paused' : ''}</td>
                    <td className="actions">
                      {acts(session) && (
                        <>
                          {session.paused ? (
                            <button type="button" onClick={() => choose('resume', session)}>
                              Resume
                            </button>
                          ) : (
                            <button type="button" onClick={() => choose('pause', session)}>
                              Pause
                            </button>
                          )}
                          <button type="button" onClick={() => choose('resend', session)}>
                            Resend
                          </button>
                        </>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    )
  }

  return (
    <>
      <h1>Sessions</h1>
      {done && <p role="status">{done}</p>}
      {live.ready ? (
        <>
          {table('client', 'Clients')}
          {table('venue', 'Venues')}
        </>
      ) : (
        <p className="muted">Loading…</p>
      )}
      {action && (
        <Reason
          title={`${action.kind[0].toUpperCase()}${action.kind.slice(1)} ${action.session.venue ?? action.session.id}`}
          onConfirm={confirm}
          onClose={() => setAction(undefined)}
        >
          {action.kind === 'resend' && (
            <label>
              From MsgSeqNum
              <input type="number" min={1} required value={from} onChange={(e) => setFrom(e.target.value)} />
            </label>
          )}
        </Reason>
      )}
    </>
  )
}
