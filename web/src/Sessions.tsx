import { useEffect, useRef, useState } from 'react'
import * as api from './api.ts'
import Reason from './Reason.tsx'
import { useLive } from './store.ts'
import type { Me, SessionView } from './types.ts'

/** How often the sequence numbers are read again, in ms: the live events don't carry them. */
export const NUMBERS_EVERY = 5_000

type Numbers = Pick<SessionView, 'next_incoming' | 'next_outgoing'>

const key = (session: Pick<SessionView, 'side' | 'id'>) => `${session.side} ${session.id}`

type Action = { kind: 'pause' | 'resume' | 'resend' | 'sequence' | 'reset'; session: SessionView }

/** A sequence number as typed: `undefined` left empty, NaN if not a whole number of at least 1. */
function seqNumber(text: string): number | undefined {
  const typed = text.trim()
  if (typed === '') return undefined
  const n = Number(typed)
  return /^\d+$/.test(typed) && Number.isSafeInteger(n) && n >= 1 ? n : NaN
}

const TITLES: Record<Action['kind'], (name: string) => string> = {
  pause: (name) => `Pause ${name}`,
  resume: (name) => `Resume ${name}`,
  resend: (name) => `Resend ${name}`,
  sequence: (name) => `Set the sequence numbers of ${name}`,
  reset: (name) => `Reset the sequence numbers of ${name}`,
}

/**
 * The client and venue sessions, live: state, next sequence numbers, paused. An operator or admin
 * may pause, resume or ask for a resend, and an admin set the sequence numbers or reset them to 1,
 * each with a reason; a scoped one only its clients', as the API allows.
 */
export default function Sessions({ me }: { me: Me }) {
  const live = useLive()
  const [action, setAction] = useState<Action>()
  const [from, setFrom] = useState('')
  const [nextIn, setNextIn] = useState('')
  const [nextOut, setNextOut] = useState('')
  const [done, setDone] = useState<string>()
  // The latest sequence numbers read, by `key`, over the store's: those change with every message,
  // and only a snapshot says them.
  const [numbers, setNumbers] = useState<Map<string, Numbers>>(new Map())
  const heading = useRef<HTMLHeadingElement>(null)
  // Set by an action done: focus goes to the heading once its dialog has closed.
  const refocus = useRef(false)

  useEffect(() => {
    if (action === undefined && refocus.current) {
      refocus.current = false
      heading.current?.focus()
    }
  }, [action])

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
    setNextIn('')
    setNextOut('')
    setAction({ kind, session })
  }

  async function confirm(reason: string) {
    if (!action) return
    const { kind, session } = action
    const name = session.venue ?? session.id
    const shown = (answer: Numbers) => {
      setNumbers((numbers) => new Map(numbers).set(key(session), answer))
      return `next in ${answer.next_incoming}, next out ${answer.next_outgoing}`
    }
    if (kind === 'sequence') {
      const [incoming, outgoing] = [seqNumber(nextIn), seqNumber(nextOut)]
      const answer = await api.sequence(session.id, reason, { next_incoming: incoming, next_outgoing: outgoing })
      setDone(`Set the sequence numbers of ${name}: ${shown(answer)}.`)
    } else if (kind === 'reset') {
      setDone(`Reset the sequence numbers of ${name}: ${shown(await api.reset(session.id, reason))}.`)
    } else if (kind === 'resend') {
      const seq = Number(from)
      if (!Number.isInteger(seq) || seq < 1) throw new Error('From must be a sequence number, 1 or more.')
      const answer = await api.resend(session.id, reason, seq)
      setNumbers((numbers) => new Map(numbers).set(key(session), answer))
      setDone(`Asked ${name} to resend from ${seq}; next incoming ${answer.next_incoming}.`)
    } else {
      await api[kind](session.id, reason)
      setDone(`${kind === 'pause' ? 'Paused' : 'Resumed'} ${name}.`)
    }
    refocus.current = true
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
                          {me.role === 'admin' && (
                            <>
                              <button type="button" onClick={() => choose('sequence', session)}>
                                Set numbers
                              </button>
                              <button type="button" onClick={() => choose('reset', session)}>
                                Reset to 1
                              </button>
                            </>
                          )}
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
      <h1 ref={heading} tabIndex={-1}>
        Sessions
      </h1>
      {/* Always there, so a screen reader hears its text change. */}
      <p role="status">{done}</p>
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
          title={TITLES[action.kind](action.session.venue ?? action.session.id)}
          onConfirm={confirm}
          onClose={() => setAction(undefined)}
          ready={action.kind !== 'sequence' || sequenceReady(nextIn, nextOut)}
          confirmLabel={action.kind === 'reset' ? 'Reset to 1' : undefined}
        >
          {action.kind === 'resend' && (
            <label>
              From MsgSeqNum
              <input type="number" min={1} required value={from} onChange={(e) => setFrom(e.target.value)} />
            </label>
          )}
          {action.kind === 'sequence' && (
            <>
              <p>Either or both: one left empty is kept.</p>
              <label>
                Next in
                <input type="number" min={1} value={nextIn} onChange={(e) => setNextIn(e.target.value)} />
              </label>
              <label>
                Next out
                <input type="number" min={1} value={nextOut} onChange={(e) => setNextOut(e.target.value)} />
              </label>
            </>
          )}
          {action.kind === 'reset' && (
            <p>
              Resets both next in and next out to 1, and clears the messages kept for resends. Only while the session
              is disconnected: pause it first.
            </p>
          )}
        </Reason>
      )}
    </>
  )
}

/** Set numbers' fields are right: one or both given, each a whole number of at least 1. */
function sequenceReady(nextIn: string, nextOut: string): boolean {
  const numbers = [seqNumber(nextIn), seqNumber(nextOut)]
  return numbers.some((n) => n !== undefined) && !numbers.some((n) => Number.isNaN(n))
}
