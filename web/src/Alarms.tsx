import { useLive } from './store.ts'
import { utc } from './time.ts'
import type { Alarm } from './types.ts'

/**
 * The alarms dismissed in this page, by identity: the store keeps each alarm's object as it came,
 * snapshots too. Not sent anywhere: another console, or this one reloaded, shows them again.
 */
export type Dismissed = ReadonlySet<Alarm>

/** A key per alarm: they have no id, but each keeps its object. */
const ids = new WeakMap<Alarm, number>()
let lastId = 0
const id = (alarm: Alarm) => ids.get(alarm) ?? (ids.set(alarm, ++lastId), lastId)


/**
 * The header's count of the alarms not dismissed, a button to their list; nothing shown when
 * there are none.
 */
export function AlarmCount({ dismissed }: { dismissed: Dismissed }) {
  const { alarms } = useLive()
  const count = alarms.filter((alarm) => !dismissed.has(alarm)).length
  // Always there, so a screen reader hears the count change, and so new alarms, without the focus
  // moving.
  return (
    <span aria-live="polite">
      {count > 0 && (
        <button type="button" className="pill warn" aria-controls="alarms" onClick={() => document.getElementById('alarms')?.focus()}>
          {count} {count === 1 ? 'alarm' : 'alarms'}
        </button>
      )}
    </span>
  )
}

/**
 * While the gateway is unavailable, a banner saying so; then the alarms not dismissed, newest
 * first. Neither takes the focus: the banner is an alert, the alarms a region to go to.
 */
export default function Alarms({ dismissed, onDismiss }: { dismissed: Dismissed; onDismiss: (now: Dismissed) => void }) {
  const { alarms, unavailable } = useLive()
  const shown = alarms.filter((alarm) => !dismissed.has(alarm))
  // Only those still kept: one past MAX_ALARMS has gone from the store, so needn't be remembered.
  const dismiss = (alarm: Alarm) => onDismiss(new Set(alarms.filter((a) => a === alarm || dismissed.has(a))))
  return (
    <>
      {unavailable && (
        <p className="banner error" role="alert">
          The gateway is unavailable: new orders are rejected until its database catches up.
        </p>
      )}
      {shown.length > 0 && (
        <section className="alarms" id="alarms" tabIndex={-1} aria-labelledby="alarms-heading">
          <h2 id="alarms-heading">Alarms</h2>
          <ul>
            {shown.map((alarm) => (
              <li key={id(alarm)}>
                <time dateTime={alarm.time}>{utc(alarm.time)}</time> <span className="kind">{alarm.kind.replaceAll('_', ' ')}</span>
                {alarm.client && <span className="client"> {alarm.client}</span>} <span className="text">{alarm.text}</span>{' '}
                <button type="button" onClick={() => dismiss(alarm)} aria-label={`Dismiss the ${alarm.kind.replaceAll('_', ' ')} alarm of ${utc(alarm.time)}`}>
                  Dismiss
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}
    </>
  )
}
