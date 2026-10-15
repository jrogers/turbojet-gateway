import type { Status } from './types.ts'

const TONES: Record<Status, string> = {
  pending_new: 'muted',
  new: 'info',
  partially_filled: 'warn',
  filled: 'ok',
  pending_cancel: 'muted',
  pending_replace: 'muted',
  canceled: 'muted',
  expired: 'error',
  rejected: 'error',
}

/** A status as the screens write it. */
export const statusText = (status: Status) => status.replaceAll('_', ' ')

/** An order's status as a pill: its text, and its tone beside it. */
export function StatusPill({ status }: { status: Status }) {
  return <span className={`pill ${TONES[status]}`}>{statusText(status)}</span>
}
