import { useEffect, useRef, useState, type FormEvent, type ReactNode } from 'react'

/**
 * A modal asking the reason for an action, which the API requires and audits. `children` are the
 * action's other fields. On submit it calls `onConfirm` with the reason, and closes once that
 * succeeds; a failure stays open, showing its message, to try again or cancel. Escape or Cancel
 * closes it, but not while the action is under way. A native modal `<dialog>`: the page behind is
 * inert. Focus starts on the reason, and goes back where it was on closing.
 */
export default function Reason({
  title,
  children,
  onConfirm,
  onClose,
}: {
  title: string
  children?: ReactNode
  onConfirm: (reason: string) => Promise<void>
  onClose: () => void
}) {
  const [reason, setReason] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const dialog = useRef<HTMLDialogElement>(null)

  useEffect(() => {
    const before = document.activeElement
    const shown = dialog.current!
    shown.showModal()
    return () => {
      shown.close()
      if (before instanceof HTMLElement) before.focus()
    }
  }, [])

  async function submit(event: FormEvent) {
    event.preventDefault()
    if (reason.trim() === '' || busy) return
    setBusy(true)
    setError(undefined)
    try {
      await onConfirm(reason.trim())
      onClose()
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
    >
      <form onSubmit={submit}>
        <h2 id="reason-title">{title}</h2>
        {children}
        <label>
          Reason
          <input autoFocus required value={reason} onChange={(e) => setReason(e.target.value)} />
        </label>
        {error && (
          <p className="error" role="alert">
            {error}
          </p>
        )}
        <div className="buttons">
          <button type="button" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={busy || reason.trim() === ''}>
            Confirm
          </button>
        </div>
      </form>
    </dialog>
  )
}
