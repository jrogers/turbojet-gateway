import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, expect, test, vi } from 'vitest'
import * as api from './api.ts'
import Reason from './Reason.tsx'
import Sessions, { NUMBERS_EVERY } from './Sessions.tsx'
import { initial, LiveContext, reduce } from './store.ts'
import type { Me, SessionView } from './types.ts'

const FUNDA = 'FIX.4.4:JETBRIDGE->FUNDA'
const VENUE = 'FIX.4.4:JETBRIDGE->NASDAQ'

const session = (side: SessionView['side'], id: string, paused = false): SessionView => ({
  side,
  id,
  venue: side === 'venue' ? 'NASDAQ' : null,
  status: 'logged_on',
  next_incoming: 12,
  next_outgoing: 34,
  activity: null,
  paused,
})

const live = reduce(initial, {
  type: 'snapshot',
  sessions: [session('client', FUNDA), session('venue', VENUE, true)],
  orders: [],
  orders_next: null,
  blocked: [],
  unavailable: false,
})

const who = (role: Me['role'], clients: string[] | null = null): Me => ({ user: 'u', role, clients })

const show = (me: Me) =>
  render(
    <LiveContext.Provider value={{ ...live, link: 'open' }}>
      <Sessions me={me} />
    </LiveContext.Provider>,
  )

const row = (name: string) => screen.getByText(name).closest('tr')!
const buttons = (name: string) => within(row(name)).queryAllByRole('button').map((b) => b.textContent)

afterEach(() => {
  vi.unstubAllGlobals()
})

test('shows clients and venues from the store', () => {
  show(who('viewer'))
  const [clients, venues] = screen.getAllByRole('table')
  expect(within(clients).getByText(FUNDA)).toBeTruthy()
  expect(within(venues).getByText('NASDAQ')).toBeTruthy()
  expect(row(FUNDA).textContent).toBe(`${FUNDA}logged on1234`)
  expect(within(row('NASDAQ')).getByText('paused')).toBeTruthy()
})

test('shows the actions by role, and a venue only to an unscoped one', () => {
  show(who('viewer'))
  expect(screen.queryAllByRole('button')).toEqual([])
  cleanup()

  show(who('operator'))
  expect(buttons(FUNDA)).toEqual(['Pause', 'Resend'])
  expect(buttons('NASDAQ')).toEqual(['Resume', 'Resend'])
  cleanup()

  show(who('admin', ['FUNDA']))
  expect(buttons(FUNDA)).toEqual(['Pause', 'Resend'])
  expect(buttons('NASDAQ')).toEqual([])
})

/** Answers each fetch with `answer`, recording the calls. */
function serve(answer: () => Response) {
  const calls: { url: string; init: RequestInit }[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit) => {
    calls.push({ url, init })
    return answer()
  })
  return calls
}

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

test('a resend posts the reason and from, with the CSRF token, and says how it went', async () => {
  let calls = serve(() => json({ user: 'u', role: 'operator', clients: null, csrf: 'tok' }))
  await api.me()
  show(who('operator'))

  calls = serve(() => json({ error: 'a resend is under way' }, 409))
  fireEvent.click(within(row(FUNDA)).getByRole('button', { name: 'Resend' }))
  const dialog = screen.getByRole('dialog')
  // Shown as a modal (the setup's stand-in for showModal opens it).
  expect((dialog as HTMLDialogElement).open).toBe(true)
  const reason = within(dialog).getByLabelText('Reason')
  expect(document.activeElement).toBe(reason)
  const confirm = within(dialog).getByRole('button', { name: 'Confirm' }) as HTMLButtonElement
  expect(confirm.disabled).toBe(true)
  fireEvent.change(reason, { target: { value: '  gap at 7  ' } })
  fireEvent.change(within(dialog).getByLabelText('From MsgSeqNum'), { target: { value: '7' } })
  fireEvent.click(confirm)
  // A refusal stays open, saying why.
  expect((await within(dialog).findByRole('alert')).textContent).toBe('a resend is under way')

  calls = serve(() => json({ next_incoming: 12, next_outgoing: 34 }))
  fireEvent.click(confirm)
  const status = await screen.findByRole('status')
  expect(status.textContent).toBe('Asked FIX.4.4:JETBRIDGE->FUNDA to resend from 7; next incoming 12.')
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(calls).toHaveLength(1)
  expect(calls[0].url).toBe('/api/sessions/FIX.4.4%3AJETBRIDGE-%3EFUNDA/resend')
  expect(calls[0].init.body).toBe('{"reason":"gap at 7","from":7}')
  expect((calls[0].init.headers as Record<string, string>)['X-CSRF-Token']).toBe('tok')
})

test('a pause posts the reason, and a resume', async () => {
  const calls = serve(() => new Response(null, { status: 204 }))
  show(who('admin'))
  fireEvent.click(within(row(FUNDA)).getByRole('button', { name: 'Pause' }))
  fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'maintenance' } })
  fireEvent.click(screen.getByRole('button', { name: 'Confirm' }))
  expect((await screen.findByRole('status')).textContent).toBe(`Paused ${FUNDA}.`)
  fireEvent.click(within(row('NASDAQ')).getByRole('button', { name: 'Resume' }))
  fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'back' } })
  fireEvent.click(screen.getByRole('button', { name: 'Confirm' }))
  expect((await screen.findByRole('status')).textContent).toBe('Resumed NASDAQ.')
  expect(calls.map((c) => [c.url, c.init.body])).toEqual([
    [`/api/sessions/${encodeURIComponent(FUNDA)}/pause`, '{"reason":"maintenance"}'],
    [`/api/sessions/${encodeURIComponent(VENUE)}/resume`, '{"reason":"back"}'],
  ])
})

test('the reason dialog needs a reason, and Escape cancels it', () => {
  const confirm = vi.fn(async () => {})
  const close = vi.fn()
  render(<Reason title="Pause" onConfirm={confirm} onClose={close} />)
  fireEvent.change(screen.getByLabelText('Reason'), { target: { value: '   ' } })
  fireEvent.submit(screen.getByRole('dialog'))
  expect(confirm).not.toHaveBeenCalled()
  // Escape is the dialog's cancel: kept from closing it natively, as the parent unmounts it.
  const cancel = new Event('cancel', { cancelable: true })
  fireEvent(screen.getByRole('dialog'), cancel)
  expect(cancel.defaultPrevented).toBe(true)
  expect(close).toHaveBeenCalledOnce()
})

test('the sequence numbers are read again while shown, and a resend gives its own', async () => {
  vi.useFakeTimers()
  try {
    const answers = [
      json([{ ...session('client', FUNDA), next_incoming: 20, next_outgoing: 40 }, session('venue', VENUE)]),
      json({ next_incoming: 21, next_outgoing: 41 }),
    ]
    const calls = serve(() => answers.shift()!)
    const { unmount } = show(who('operator'))
    expect(row(FUNDA).textContent).toBe(`${FUNDA}logged on1234PauseResend`)
    await act(() => vi.advanceTimersByTimeAsync(NUMBERS_EVERY))
    expect(calls.map((c) => c.url)).toEqual(['/api/sessions'])
    expect(row(FUNDA).textContent).toBe(`${FUNDA}logged on2040PauseResend`)

    fireEvent.click(within(row(FUNDA)).getByRole('button', { name: 'Resend' }))
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'gap' } })
    fireEvent.change(screen.getByLabelText('From MsgSeqNum'), { target: { value: '3' } })
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Confirm' })))
    expect(row(FUNDA).textContent).toBe(`${FUNDA}logged on2141PauseResend`)

    unmount()
    await vi.advanceTimersByTimeAsync(NUMBERS_EVERY * 3)
    expect(calls).toHaveLength(2)
  } finally {
    vi.useRealTimers()
  }
})
