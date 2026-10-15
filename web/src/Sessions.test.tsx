import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import * as api from './api.ts'
import Reason from './Reason.tsx'
import Sessions, { NUMBERS_EVERY } from './Sessions.tsx'
import { initial, LiveContext, reduce } from './store.ts'
import type { Me, OrderView, SessionView } from './types.ts'

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
  clients: null,
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

/**
 * Answers each fetch with `answer`, recording the calls, but the sequence numbers' reads: those
 * answer the store's sessions, unrecorded.
 */
function serve(answer: () => Response) {
  const calls: { url: string; init: RequestInit }[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit) => {
    if (url === '/api/sessions') return json(live.sessions)
    calls.push({ url, init })
    return answer()
  })
  return calls
}

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

beforeEach(() => {
  serve(() => new Response(null, { status: 500 }))
})

test('shows clients and venues from the store', () => {
  show(who('viewer'))
  const [clients, venues] = screen.getAllByRole('table')
  expect(within(clients).getByText(FUNDA)).toBeTruthy()
  expect(within(venues).getByText('NASDAQ')).toBeTruthy()
  expect(row(FUNDA).textContent).toBe(`${FUNDA}logged on1234`)
  expect(within(row('NASDAQ')).getByText('paused')).toBeTruthy()
})

test('sums the sessions, orders and blocks up in cards, the venues down in warn', () => {
  const cards = () =>
    [...document.querySelectorAll('.card')].map((card) => [card.querySelector('dt')!.textContent, card.querySelector('dd')!.textContent])
  const venues = () => document.querySelectorAll('.card dd')[1]
  show(who('viewer'))
  // NASDAQ's paused: not down.
  expect(cards()).toEqual([
    ['Clients logged on', '1 / 1'],
    ['Venues logged on', '1 / 1'],
    ['Working orders', '0'],
    ['Blocks', '0'],
  ])
  expect(venues().className).toBe('')
  cleanup()

  const down = { ...session('venue', 'FIX.4.4:JETBRIDGE->NYSE'), venue: 'NYSE', status: 'disconnected' as const }
  // The store counts what the snapshot has; their fields are the blotter's.
  const orders = [1, 2].map((id) => ({ id }) as OrderView)
  const state = reduce(initial, {
    type: 'snapshot',
    sessions: [
      session('client', FUNDA),
      { ...session('client', 'FIX.4.4:JETBRIDGE->FUNDB'), status: 'connected' },
      session('venue', VENUE),
      down,
    ],
    orders,
    orders_next: 3,
    blocked: ['all', 'comp:FUNDA'],
    unavailable: false,
    clients: null,
  })
  render(
    <LiveContext.Provider value={{ ...state, link: 'open' }}>
      <Sessions me={who('viewer')} />
    </LiveContext.Provider>,
  )
  expect(cards()).toEqual([
    ['Clients logged on', '1 / 2'],
    ['Venues logged on', '1 / 2, 1 down'],
    ['Working orders', '2+'],
    ['Blocks', '2'],
  ])
  expect(venues().className).toBe('warn')
  // Each state a pill, toned by it.
  const pill = (name: string) => row(name).querySelector('.pill')!
  expect([pill(FUNDA), pill('FIX.4.4:JETBRIDGE->FUNDB'), pill('NYSE')].map((p) => [p.className, p.textContent])).toEqual([
    ['pill ok', 'logged on'],
    ['pill info', 'connected'],
    ['pill warn', 'disconnected'],
  ])
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
  expect(buttons(FUNDA)).toEqual(['Pause', 'Resend', 'Set numbers…', 'Reset to 1…'])
  expect(buttons('NASDAQ')).toEqual([])
  cleanup()

  show(who('admin'))
  expect(buttons('NASDAQ')).toEqual(['Resume', 'Resend', 'Set numbers…', 'Reset to 1…'])
})

test('set numbers takes either or both, each 1 or more, and shows the answer in the row', async () => {
  const calls = serve(() => json({ next_incoming: 12, next_outgoing: 100 }))
  show(who('admin'))
  fireEvent.click(within(row(FUNDA)).getByRole('button', { name: 'Set numbers…' }))
  const dialog = screen.getByRole('dialog')
  expect(within(dialog).getByRole('heading').textContent).toBe(`Set the sequence numbers of ${FUNDA}`)
  fireEvent.change(within(dialog).getByLabelText('Reason'), { target: { value: 'agreed with FUNDA' } })
  const confirm = within(dialog).getByRole('button', { name: 'Confirm' }) as HTMLButtonElement
  // Neither given yet.
  expect(confirm.disabled).toBe(true)
  const nextIn = within(dialog).getByLabelText('Next in')
  const nextOut = within(dialog).getByLabelText('Next out')
  const hint = 'Each is a whole number, 1 or more.'
  expect(within(dialog).queryByText(hint)).toBeNull()
  expect(dialog.textContent).toContain('pause the session first to be safe')
  fireEvent.change(nextOut, { target: { value: '0' } })
  expect(confirm.disabled).toBe(true)
  expect(within(dialog).getByText(hint)).toBeTruthy()
  // Typed as is, not read as empty: a browser's number field would hand the code ''.
  fireEvent.change(nextOut, { target: { value: '1e2' } })
  expect(confirm.disabled).toBe(true)
  expect(within(dialog).getByText(hint)).toBeTruthy()
  fireEvent.change(nextOut, { target: { value: '100' } })
  fireEvent.change(nextIn, { target: { value: '1.5' } })
  expect(confirm.disabled).toBe(true)
  fireEvent.change(nextIn, { target: { value: '' } })
  expect(confirm.disabled).toBe(false)
  expect(within(dialog).queryByText(hint)).toBeNull()
  await act(async () => fireEvent.click(confirm))
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(screen.getByRole('status').textContent).toBe(`Set the sequence numbers of ${FUNDA}: next in 12, next out 100.`)
  expect(row(FUNDA).textContent).toBe(`${FUNDA}logged on12100PauseResendSet numbers…Reset to 1…`)
  expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1 }))
  expect(calls.map((c) => [c.url, c.init.body])).toEqual([
    [`/api/sessions/${encodeURIComponent(FUNDA)}/sequence`, '{"reason":"agreed with FUNDA","next_outgoing":100}'],
  ])
})

test('reset to 1 warns that it resets both, and says why it was refused', async () => {
  let calls = serve(() => json({ error: 'the session is connected; disconnect it first' }, 409))
  show(who('admin'))
  fireEvent.click(within(row('NASDAQ')).getByRole('button', { name: 'Reset to 1…' }))
  const dialog = screen.getByRole('dialog')
  expect(dialog.textContent).toContain('Resets both next in and next out to 1')
  fireEvent.change(within(dialog).getByLabelText('Reason'), { target: { value: 'new week' } })
  const confirm = within(dialog).getByRole('button', { name: 'Reset to 1' })
  fireEvent.click(confirm)
  expect((await within(dialog).findByRole('alert')).textContent).toBe('the session is connected; disconnect it first')

  calls = serve(() => json({ next_incoming: 1, next_outgoing: 1 }))
  fireEvent.click(confirm)
  expect((await screen.findByRole('status')).textContent).toBe('Reset the sequence numbers of NASDAQ: next in 1, next out 1.')
  expect(row('NASDAQ').textContent).toBe('NASDAQlogged on11pausedResumeResendSet numbers…Reset to 1…')
  expect(calls.map((c) => [c.url, c.init.body])).toEqual([[`/api/sessions/${encodeURIComponent(VENUE)}/reset`, '{"reason":"new week"}']])
})


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

test('the sequence numbers are read on showing, then again while the page is seen', async () => {
  vi.useFakeTimers()
  try {
    const reads = [
      [{ ...session('client', FUNDA), next_incoming: 20, next_outgoing: 40 }, session('venue', VENUE)],
      [{ ...session('client', FUNDA), next_incoming: 22, next_outgoing: 42 }, session('venue', VENUE)],
    ]
    const calls: string[] = []
    vi.stubGlobal('fetch', async (url: string) => {
      calls.push(url)
      return url === '/api/sessions' ? json(reads.shift()) : json({ next_incoming: 21, next_outgoing: 41 })
    })
    let hidden = false
    vi.spyOn(document, 'hidden', 'get').mockImplementation(() => hidden)
    const { unmount } = show(who('operator'))
    await act(() => vi.advanceTimersByTimeAsync(0))
    expect(calls).toEqual(['/api/sessions'])
    expect(row(FUNDA).textContent).toBe(`${FUNDA}logged on2040PauseResend`)

    // A resend's answer is shown at once.
    fireEvent.click(within(row(FUNDA)).getByRole('button', { name: 'Resend' }))
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'gap' } })
    fireEvent.change(screen.getByLabelText('From MsgSeqNum'), { target: { value: '3' } })
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Confirm' })))
    expect(row(FUNDA).textContent).toBe(`${FUNDA}logged on2141PauseResend`)

    // Not read while the page is hidden.
    hidden = true
    await act(() => vi.advanceTimersByTimeAsync(NUMBERS_EVERY * 2))
    expect(calls).toHaveLength(2)
    hidden = false
    await act(() => vi.advanceTimersByTimeAsync(NUMBERS_EVERY))
    expect(calls).toHaveLength(3)
    expect(row(FUNDA).textContent).toBe(`${FUNDA}logged on2242PauseResend`)

    unmount()
    await vi.advanceTimersByTimeAsync(NUMBERS_EVERY * 3)
    expect(calls).toHaveLength(3)
  } finally {
    vi.useRealTimers()
    vi.restoreAllMocks()
  }
})

test('a dialog the browser closes is closed for the parent too, but not by its own unmounting', () => {
  const close = vi.fn()
  const { unmount } = render(<Reason title="Pause" onConfirm={async () => {}} onClose={close} />)
  const dialog = screen.getByRole('dialog') as HTMLDialogElement
  dialog.open = false
  fireEvent(dialog, new Event('close'))
  expect(close).toHaveBeenCalledOnce()
  unmount()
  fireEvent(dialog, new Event('close'))
  expect(close).toHaveBeenCalledOnce()
})

test('an action done after the browser closed its dialog closes nothing more', async () => {
  let succeed = () => {}
  const confirm = () => new Promise<void>((resolve) => (succeed = resolve))
  const close = vi.fn()
  render(<Reason title="Pause" onConfirm={confirm} onClose={close} />)
  fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'why' } })
  fireEvent.submit(screen.getByRole('dialog'))
  const dialog = screen.getByRole('dialog') as HTMLDialogElement
  dialog.open = false
  fireEvent(dialog, new Event('close'))
  expect(close).toHaveBeenCalledOnce()
  // The parent unmounts it, as it would, then the action succeeds.
  cleanup()
  await act(async () => succeed())
  expect(close).toHaveBeenCalledOnce()
})
