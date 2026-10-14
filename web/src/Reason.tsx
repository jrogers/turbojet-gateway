import { useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react'

/**
 * A modal asking the reason for an action, which the API requires and audits. `children` are the
 * action's other fields. On submit it calls `onConfirm` with the reason, and closes once that
 * succeeds; a failure stays open, showing its message, to try again or cancel. Escape or Cancel
 * closes it, but not while the action is under way; if the browser closes it anyway, the parent is
 * told. A native modal `<dialog>`: the page behind is inert. Focus starts on the reason, and goes
 * back where it was on closing. `children` come after the reason, so it's typed first. `ready`
 * false keeps Confirm disabled, as the children say; `confirmLabel` names it. `onClose` should be
 * idempotent, as setting the parent's state to none is: closings can race, the browser's and the
 * component's own.
 */
export default function Reason({
  title,
  children,
  onConfirm,
  onClose,
  ready = true,
  confirmLabel = 'Confirm',
}: {
  title: string
  children?: ReactNode
  ready?: boolean
  confirmLabel?: string
  onConfirm: (reason: string) => Promise<void>
  onClose: () => void
}) {
  const [reason, setReason] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const dialog = useRef<HTMLDialogElement>(null)
  // Set as it unmounts: its own close() fires `close` too, later, which mustn't close the parent's
  // next action.
  const gone = useRef(false)

  useEffect(() => {
    const before = document.activeElement
    const shown = dialog.current!
    gone.current = false
    shown.showModal()
    return () => {
      gone.current = true
      shown.close()
      if (before instanceof HTMLElement) before.focus()
    }
  }, [])

  async function submit(event: FormEvent) {
    event.preventDefault()
    if (reason.trim() === '' || busy || !ready) return
    setBusy(true)
    setError(undefined)
    try {
      await onConfirm(reason.trim())
      // Not if closed meanwhile, by the browser: the parent may have a new action open.
      if (!gone.current) onClose()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      setBusy(false)
    }
  }

  return (
    <dialog
      ref={dialog}
      className="dialog"
      aria-labelledby="reason-title"
      onCancel={(e) => {
        // Escape: closing is the parent's to do, by unmounting.
        e.preventDefault()
        if (!busy) onClose()
      }}
      // The browser may close it regardless (Chrome does on a second Escape): unmount it then, or
      // the next action would reuse it, closed. Not for a close of its own: unmounting, or
      // StrictMode's remount, which has shown it again by the time the event comes.
      onClose={() => {
        if (!gone.current && !dialog.current?.open) onClose()
      }}
    >
      <form onSubmit={submit}>
        <h2 id="reason-title">{title}</h2>
        <label>
          Reason
          <input autoFocus required value={reason} onChange={(e) => setReason(e.target.value)} />
        </label>
        {children}
        {error && (
          <p className="error" role="alert">
            {error}
          </p>
        )}
        <div className="buttons">
          <button type="button" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={busy || reason.trim() === '' || !ready}>
            {confirmLabel}
          </button>
        </div>
      </form>
    </dialog>
  )
}
