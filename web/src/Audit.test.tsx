import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, expect, test, vi } from 'vitest'
import Audit from './Audit.tsx'
import type { AuditEntry } from './types.ts'

const entry = (time: string, action: string, more: Partial<AuditEntry> = {}): AuditEntry => ({
  time,
  user: 'alice',
  role: 'operator',
  ip: '192.0.2.1',
  action,
  target: 'comp:FUNDA',
  params: null,
  reason: 'runaway algo',
  result: 'allowed',
  ...more,
})

const ENTRIES = [
  entry('2026-10-11T09:30:00.123456Z', 'block'),
  entry('2026-10-11T09:31:00Z', 'sequence', {
    target: 'FIX.4.4:JETBRIDGE->FUNDA',
    params: { next_incoming: null, next_outgoing: 100 },
  }),
  entry('2026-10-11T09:32:00Z', 'login', { user: null, role: null, target: null, reason: null, result: 'refused: wrong password' }),
]

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

// Fetches no route answers: `request()` would hide the throw as unreachable.
let stray: string[] = []

/** Answers fetches by URL from `routes`, the default read's entries by default, recording them. */
function serve(routes: Record<string, () => Response> = {}) {
  const calls: string[] = []
  const all: Record<string, () => Response> = { '/api/audit?limit=100': () => json(ENTRIES), ...routes }
  vi.stubGlobal('fetch', async (url: string) => {
    calls.push(url)
    const answer = all[url]
    if (answer === undefined) {
      stray.push(url)
      throw new Error(`unexpected fetch: ${url}`)
    }
    return answer()
  })
  return calls
}

beforeEach(() => {
  serve()
})

afterEach(() => {
  vi.unstubAllGlobals()
  const seen = stray
  stray = []
  expect(seen).toEqual([])
})

const rows = () => screen.getAllByRole('row').slice(1).map((row) => Array.from((row as HTMLTableRowElement).cells).map((cell) => cell.textContent))

test('shows the last entries newest first, params as compact JSON, missing fields empty', async () => {
  render(<Audit />)
  await screen.findByRole('table')
  expect(rows()).toEqual([
    ['2026-10-11 09:32:00', '', '', '192.0.2.1', 'login', '', '', '', 'refused: wrong password'],
    [
      '2026-10-11 09:31:00',
      'alice',
      'operator',
      '192.0.2.1',
      'sequence',
      'FIX.4.4:JETBRIDGE->FUNDA',
      '{"next_incoming":null,"next_outgoing":100}',
      'runaway algo',
      'allowed',
    ],
    ['2026-10-11 09:30:00', 'alice', 'operator', '192.0.2.1', 'block', 'comp:FUNDA', '', 'runaway algo', 'allowed'],
  ])
  expect(screen.getByText('3, newest first.')).toBeTruthy()
})

test('since and limit narrow the read, and a full read says older ones are left out', async () => {
  const calls = serve({
    '/api/audit?since=2026-10-11T09%3A31%3A00.000Z&limit=2': () => json(ENTRIES.slice(1)),
    '/api/audit?limit=1000': () => json([]),
  })
  render(<Audit />)
  await screen.findByRole('table')
  fireEvent.change(screen.getByLabelText('Since (UTC; the start if empty)'), { target: { value: '2026-10-11T09:31' } })
  fireEvent.change(screen.getByLabelText('Limit'), { target: { value: '2' } })
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Read' })))
  expect(rows().map((row) => row[4])).toEqual(['login', 'sequence'])
  expect(screen.getByText('The last 2, newest first: older ones are left out; a larger limit reaches them.')).toBeTruthy()

  fireEvent.change(screen.getByLabelText('Since (UTC; the start if empty)'), { target: { value: '' } })
  fireEvent.change(screen.getByLabelText('Limit'), { target: { value: '1000' } })
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Read' })))
  expect(screen.getByText('None.')).toBeTruthy()
  expect(calls).toEqual(['/api/audit?limit=100', '/api/audit?since=2026-10-11T09%3A31%3A00.000Z&limit=2', '/api/audit?limit=1000'])
})

test('a limit out of bounds is said, and nothing asked', async () => {
  const calls = serve()
  render(<Audit />)
  await screen.findByRole('table')
  fireEvent.change(screen.getByLabelText('Limit'), { target: { value: '1001' } })
  // Submitted past the browser's own check, which a click would stop at.
  fireEvent.submit(screen.getByRole('button', { name: 'Read' }).closest('form')!)
  expect(screen.getByRole('alert').textContent).toBe('Limit is 1 to 1000.')
  expect(screen.getByRole('table')).toBeTruthy()
  expect(calls).toEqual(['/api/audit?limit=100'])
})

test('a refusal says why and clears the entries', async () => {
  let refuse = false
  serve({
    '/api/audit?limit=100': () => (refuse ? json({ error: 'u may see only clients FUNDA, not the audit log' }, 403) : json(ENTRIES)),
  })
  render(<Audit />)
  await screen.findByRole('table')
  refuse = true
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Read' })))
  expect(screen.getByRole('alert').textContent).toBe('u may see only clients FUNDA, not the audit log')
  expect(screen.queryByRole('table')).toBeNull()
})
