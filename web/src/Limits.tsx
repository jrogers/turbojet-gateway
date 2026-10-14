import { useEffect, useReducer, useRef, useState, type RefObject } from 'react'
import * as api from './api.ts'
import Reason from './Reason.tsx'
import { useLive } from './store.ts'
import type { EditedView, LimitsSet, Me, ReloadView, RiskTable, RiskTables } from './types.ts'

const KEYS = ['max_order_qty', 'max_notional', 'symbols', 'price_band', 'credit', 'self_trade'] as const
type Key = (typeof KEYS)[number]

const LABELS: Record<Key, string> = {
  max_order_qty: 'Max order qty',
  max_notional: 'Max notional',
  symbols: 'Symbols',
  price_band: 'Price band %',
  credit: 'Credit',
  self_trade: 'Self trade',
}

/** A decimal as typed: a sign is let through, so the server's "is not positive" says why. */
const DECIMAL = /^[+-]?(\d+\.?\d*|\.\d+)$/

/** What an admin is about to do: `table` is a CompID, or `null` for the defaults. */
type Editing = { kind: 'set'; table: string | null } | { kind: 'delete'; table: string }

/** What the last edit answered, and of which table. */
type Answer = { table: string | null; edited: EditedView }

const nameOf = (table: string | null) => (table === null ? 'the defaults' : table)
const numeric = (key: Key) => (key === 'symbols' || key === 'self_trade' ? undefined : 'num')

/** A key's value as written, as text: an amount as the API's decimal string. */
function shown(key: Key, table: RiskTable): string {
  const value = table[key]
  if (value === undefined) return ''
  if (key === 'self_trade') return value ? 'refused' : 'allowed'
  return Array.isArray(value) ? value.join(', ') : String(value)
}

/**
 * The value an edit sends, or `undefined` while what's typed isn't one: `null` to remove the key,
 * a decimal as a string, the symbols as a list, `self_trade` a boolean.
 */
function valueOf(key: Key, text: string, on: boolean, remove: boolean): LimitsSet[Key] | undefined {
  if (remove) return null
  if (key === 'self_trade') return on
  if (key === 'symbols') {
    const symbols = text.split(/[\s,]+/).filter((s) => s !== '')
    return symbols.length === 0 ? undefined : symbols
  }
  return DECIMAL.test(text.trim()) ? text.trim() : undefined
}

/**
 * The risk limits as `jetbridge.toml` writes them (`GET /api/risk/limits`), the defaults and each
 * client's table with only the keys it sets: the others say "default". An admin edits one key at
 * a time, or removes it, and deletes or adds a client's table; the defaults only an admin without
 * clients, a client's table an admin of it, as the API allows. Each edit is read again, with the
 * risk (`onDone`), and its answer shown: the table, and what the reload took when the API says.
 */
export default function Limits({
  me,
  reloads,
  heading,
  onDone,
}: {
  /** Its clients as the last snapshot said, as the shell merges them. */
  me: Me
  /** Counts the console's reloads: each reads the tables again. */
  reloads: number
  /** Focused after an edit. */
  heading: RefObject<HTMLHeadingElement | null>
  /** Says how an edit went, and reads the risk again. */
  onDone: (text: string) => void
}) {
  const live = useLive()
  // `undefined` until read; `null` after a 403 or 404, with `error` saying why.
  const [tables, setTables] = useState<RiskTables | null>()
  const [error, setError] = useState<string>()
  const [editing, setEditing] = useState<Editing>()
  const [key, setKey] = useState<Key>(KEYS[0])
  // `undefined` until changed: the table's value then, as last read.
  const [text, setText] = useState<string>()
  const [on, setOn] = useState<boolean>()
  const [remove, setRemove] = useState(false)
  const [adding, setAdding] = useState('')
  // Why Add refused the CompID typed: not one of a scoped admin's clients.
  const [refused, setRefused] = useState<string>()
  const [answer, setAnswer] = useState<Answer>()
  const refocus = useRef(false)
  const [reads, reread] = useReducer((n: number) => n + 1, 0)

  // On showing, on being seen again, on each snapshot (the scope may have changed) and console
  // reload, and on each edit opened or done. An answer to a read since outdone, or after
  // unmounting, is dropped. A 403 or 404 clears the tables, and any edit open on them.
  const asking = useRef<object>(undefined)
  useEffect(() => {
    const read = () => {
      if (document.hidden) return
      const at = {}
      asking.current = at
      api.riskLimits().then(
        (tables) => {
          if (asking.current !== at) return
          setTables(tables)
          setError(undefined)
        },
        (e: unknown) => {
          if (asking.current !== at) return
          if (e instanceof api.ApiError && (e.status === 403 || e.status === 404)) {
            setTables(null)
            setAnswer(undefined)
            // Heard only if an edit was open: closing it runs the focus effect. If none was,
            // `open` clears the flag before the next one.
            refocus.current = true
            setEditing(undefined)
          }
          setError(api.message(e))
        },
      )
    }
    read()
    document.addEventListener('visibilitychange', read)
    return () => {
      document.removeEventListener('visibilitychange', read)
      asking.current = undefined
    }
  }, [live.snapshots, reloads, reads])

  useEffect(() => {
    if (editing === undefined && refocus.current) {
      refocus.current = false
      heading.current?.focus()
    }
  }, [editing, heading])

  const admin = me.role === 'admin'
  const { clients } = me
  const mayEdit = (table: string | null) => admin && (clients === null || (table !== null && clients.includes(table)))
  const tableOf = (table: string | null): RiskTable => (table === null ? tables?.defaults : tables?.clients[table]) ?? {}

  const choose = (key: Key) => {
    setKey(key)
    setText(undefined)
    setOn(undefined)
    setRemove(false)
  }
  const open = (editing: Editing) => {
    refocus.current = false
    choose(KEYS[0])
    setEditing(editing)
    reread()
  }

  // The fields as typed, else as the table has them, as last read; `self_trade` a client leaves
  // out as the defaults have it.
  const own = editing?.kind === 'set' ? tableOf(editing.table) : {}
  const sets = own[key] !== undefined
  const inherited = editing?.table != null && !sets && key === 'self_trade' ? (tables?.defaults.self_trade ?? false) : undefined
  const typed = text ?? shown(key, own)
  const ticked = on ?? own.self_trade ?? inherited ?? false
  // A `self_trade` the table doesn't set sends nothing until clicked: confirming the box as shown
  // would pin the default's value as the client's own.
  const untouched = key === 'self_trade' && !sets && on === undefined
  const value = editing?.kind === 'set' && !untouched ? valueOf(key, typed, ticked, remove && sets) : undefined

  async function confirm(reason: string) {
    if (!editing) return
    const name = nameOf(editing.table)
    let edited: EditedView
    let said: string
    if (editing.kind === 'delete') {
      edited = await api.deleteClientLimits(editing.table, reason)
      said = `Deleted ${name}'s limits: its sessions have the defaults.`
    } else {
      if (value === undefined) return
      const set: LimitsSet = { [key]: value }
      edited =
        editing.table === null ? await api.setDefaultLimits(reason, set) : await api.setClientLimits(editing.table, reason, set)
      said = value === null ? `Removed ${key} from ${name}.` : `Set ${key} in ${name}.`
      setAdding('')
    }
    setAnswer({ table: editing.table, edited })
    refocus.current = true
    reread()
    onDone(said)
  }

  if (tables === undefined) return error ? <Failed error={error} /> : <p className="muted">Loading…</p>
  if (tables === null) return <Failed error={error} />
  const rows: [string | null, RiskTable][] = [[null, tables.defaults], ...Object.entries(tables.clients)]

  return (
    <>
      {error && <Failed error={error} />}
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Table</th>
              {KEYS.map((key) => (
                <th key={key} className={numeric(key)}>
                  {LABELS[key]}
                </th>
              ))}
              {admin && (
                <th>
                  <span className="visually-hidden">Actions</span>
                </th>
              )}
            </tr>
          </thead>
          <tbody>
            {rows.map(([table, limits]) => (
              <tr key={table ?? ''}>
                <th scope="row">{table ?? 'Defaults'}</th>
                {KEYS.map((key) => (
                  <td key={key} className={numeric(key)}>
                    {/* A client's table leaving a key out has the default's. */}
                    {table !== null && limits[key] === undefined ? <span className="muted">default</span> : shown(key, limits)}
                  </td>
                ))}
                {admin && (
                  <td className="actions">
                    {mayEdit(table) && (
                      <button type="button" aria-label={`Edit ${nameOf(table)}`} onClick={() => open({ kind: 'set', table })}>
                        Edit…
                      </button>
                    )}
                    {table !== null && mayEdit(table) && (
                      <button type="button" aria-label={`Delete ${table}'s limits`} onClick={() => open({ kind: 'delete', table })}>
                        Delete table…
                      </button>
                    )}
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {admin && (
        <form
          className="filters"
          onSubmit={(e) => {
            e.preventDefault()
            const table = adding.trim()
            if (table === '') return
            if (!mayEdit(table)) return setRefused(`You may edit only your clients' limits: ${clients!.join(', ')}.`)
            open({ kind: 'set', table })
          }}
        >
          <label>
            Add client
            <input
              placeholder="CompID"
              list={clients === null ? undefined : 'limit-clients'}
              value={adding}
              aria-invalid={refused !== undefined || undefined}
              aria-describedby={refused === undefined ? undefined : 'add-refused'}
              onChange={(e) => {
                setAdding(e.target.value)
                setRefused(undefined)
              }}
            />
          </label>
          {clients !== null && (
            <datalist id="limit-clients">
              {clients.map((client) => (
                <option key={client} value={client} />
              ))}
            </datalist>
          )}
          <button type="submit" disabled={adding.trim() === ''}>
            Add…
          </button>
        </form>
      )}
      {refused && (
        <p className="error" role="alert" id="add-refused">
          {refused}
        </p>
      )}

      {answer && <Answered answer={answer} />}

      {editing && (
        <Reason
          title={editing.kind === 'delete' ? `Delete ${editing.table}'s limits` : `Edit ${nameOf(editing.table)}`}
          onConfirm={confirm}
          onClose={() => setEditing(undefined)}
          ready={editing.kind === 'delete' || value !== undefined}
          confirmLabel={editing.kind === 'delete' ? 'Delete table' : undefined}
        >
          {editing.kind === 'delete' ? (
            <p>Its sessions get the defaults.</p>
          ) : (
            <>
              <label>
                Key
                <select value={key} onChange={(e) => choose(e.target.value as Key)}>
                  {KEYS.map((key) => (
                    <option key={key} value={key}>
                      {LABELS[key]}
                    </option>
                  ))}
                </select>
              </label>
              {/* Only a key the table sets can be removed. */}
              <label className="check">
                <input type="checkbox" disabled={!sets} checked={remove && sets} onChange={(e) => setRemove(e.target.checked)} />
                {editing.table === null ? 'Remove it: no check' : 'Remove it: the default applies'}
              </label>
              {!(remove && sets) &&
                (key === 'self_trade' ? (
                  <label className="check">
                    <input type="checkbox" checked={ticked} onChange={(e) => setOn(e.target.checked)} />
                    Refuse self trades{' '}
                    {inherited !== undefined && <span className="muted">(default: {inherited ? 'refused' : 'allowed'})</span>}
                  </label>
                ) : (
                  <label>
                    {key === 'symbols' ? 'Symbols, separated by commas' : 'Value, a positive decimal'}
                    <input
                      autoComplete="off"
                      spellCheck={false}
                      inputMode={key === 'symbols' ? undefined : 'decimal'}
                      value={typed}
                      onChange={(e) => setText(e.target.value)}
                    />
                  </label>
                ))}
            </>
          )}
        </Reason>
      )}
    </>
  )
}

function Failed({ error }: { error?: string }) {
  return (
    <p className="error" role="alert">
      {error}
    </p>
  )
}

/** The table as the file now has it, and what the reload took, if the API said. */
function Answered({ answer: { table, edited } }: { answer: Answer }) {
  const [subject, verb] = table === null ? ['The defaults', 'set'] : [table, 'sets']
  const keys = edited.table === null ? [] : KEYS.filter((key) => edited.table![key] !== undefined)
  return (
    <div className="answer">
      <p>
        {edited.table === null
          ? `${subject} has no table now.`
          : keys.length === 0
            ? `${subject} ${verb} no keys now.`
            : `${subject} now ${verb} ${keys.map((key) => `${key} ${shown(key, edited.table!)}`).join('; ')}.`}
      </p>
      {edited.reload && <Reloaded reload={edited.reload} />}
    </div>
  )
}

/** What a reload took: the config's summary, the sessions changed, and its warnings. */
export function Reloaded({ reload: { sessions, config, warnings } }: { reload: ReloadView }) {
  const changes = Object.entries(sessions)
    .filter(([, names]) => names.length > 0)
    .map(([what, names]) => `${what.replaceAll('_', ' ')} ${names.join(', ')}`)
  return (
    <>
      <p>
        The reload took {config.routes} routes, limits for {config.clients} clients and day_end {config.day_end}; sessions:{' '}
        {changes.length === 0 ? 'nothing changed' : changes.join('; ')}.
      </p>
      {warnings.length > 0 && (
        <ul className="warnings">
          {warnings.map((warning, i) => (
            <li key={i}>{warning}</li>
          ))}
        </ul>
      )}
    </>
  )
}
