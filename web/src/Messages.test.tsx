import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, expect, test, vi } from 'vitest'
import Messages, { CONDITIONS_MAX, searchParams, type Search } from './Messages.tsx'
import { initial, LiveContext, reduce } from './store.ts'
import type { Hit, Me, Page, SessionView } from './types.ts'

const FUNDA = 'FIX.4.4:JETBRIDGE->FUNDA'
const VENUE = 'FIX.4.4:JETBRIDGE->NASDAQ'

const session = (side: SessionView['side'], id: string): SessionView => ({
  side,
  id,
  venue: side === 'venue' ? 'NASDAQ' : null,
  status: 'logged_on',
  next_incoming: null,
  next_outgoing: null,
  activity: null,
  paused: false,
})

const live = reduce(initial, {
  type: 'snapshot',
  sessions: [session('client', FUNDA), session('venue', VENUE)],
  orders: [],
  orders_next: null,
  blocked: [],
  unavailable: false,
  clients: null,
})

const who = (clients: string[] | null = null): Me => ({ user: 'u', role: 'viewer', clients })

const hit = (raw: string): Hit => ({
  time: '2026-10-14T09:00:07.123456Z',
  direction: 'in',
  session: FUNDA,
  msg_type: 'D',
  fields: [[35, 'D']],
  raw,
})

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

// Fetches no route answers: `request()` would hide the throw as unreachable.
let stray: string[] = []

/** Answers fetches by URL from `routes`, recording them: the screen only ever GETs. */
function serve(routes: Record<string, () => Response>) {
  const calls: string[] = []
  vi.stubGlobal('fetch', async (url: string) => {
    calls.push(url)
    const answer = routes[url]
    if (answer === undefined) {
      stray.push(url)
      throw new Error(`unexpected fetch: ${url}`)
    }
    return answer()
  })
  return calls
}

afterEach(() => {
  vi.unstubAllGlobals()
  const seen = stray
  stray = []
  expect(seen).toEqual([])
})

const show = (me = who()) =>
  render(
    <LiveContext.Provider value={{ ...live, link: 'open' }}>
      <Messages me={me} />
    </LiveContext.Provider>,
  )

const type = (label: string, value: string, at = 0) => fireEvent.change(screen.getAllByLabelText(label)[at], { target: { value } })

/** Submits the form and lets its first read answer. */
const searchNow = () => act(async () => fireEvent.click(screen.getByRole('button', { name: 'Search' })))

const rawShown = () => [...document.querySelectorAll('table.messages code')].map((code) => code.textContent)

const SINCE = 'since=2026-10-08T09%3A00%3A00.000Z'
const UNTIL = 'until=2026-10-09T09%3A00%3A00.000Z'

const blank: Search = { session: '', direction: '', msgTypes: '', tags: [], since: '2026-10-08T09:00', until: '', limit: '' }

test('the query repeats tag, lists the MsgTypes, and gives the times as UTC', () => {
  expect(
    searchParams({
      ...blank,
      session: ` ${FUNDA} `,
      direction: 'out',
      msgTypes: 'D, F,,8',
      tags: [
        ['11', 'A=B'],
        ['', ''],
        [' 55', 'MSFT'],
      ],
      since: '2026-10-08T09:00:30',
      until: '2026-10-08T17:00',
      limit: '50',
    }),
  ).toEqual([
    ['session', FUNDA],
    ['direction', 'out'],
    ['msg_type', 'D,F,8'],
    ['tag', '11=A=B'],
    ['tag', '55=MSFT'],
    ['since', '2026-10-08T09:00:30.000Z'],
    ['until', '2026-10-08T17:00:00.000Z'],
    ['limit', '50'],
  ])
  // Until defaults to a day after since.
  expect(searchParams(blank)).toEqual([
    ['since', '2026-10-08T09:00:00.000Z'],
    ['until', '2026-10-09T09:00:00.000Z'],
  ])
})

test('the range and the conditions are checked before asking', () => {
  expect(searchParams({ ...blank, since: '' })).toBe('Since is required.')
  expect(searchParams({ ...blank, until: '2026-10-09T09:00:01' })).toBe('A search covers 24 hours at most.')
  expect(searchParams({ ...blank, until: '2026-10-09T09:00' })).not.toBeTypeOf('string')
  expect(searchParams({ ...blank, until: '2026-10-08T09:00' })).toBe('Until must be after since.')
  expect(searchParams({ ...blank, tags: [['x', '1']] })).toBe('Tag condition x=1 needs a tag number and a value.')
  expect(searchParams({ ...blank, tags: [['11', '']] })).toBe('Tag condition 11= needs a tag number and a value.')
  expect(searchParams({ ...blank, limit: '1001' })).toBe('Limit is 1 to 1000.')
  const tags = Array.from({ length: CONDITIONS_MAX }, (_, i): [string, string] => [String(i + 1), 'x'])
  expect(searchParams({ ...blank, tags })).not.toBeTypeOf('string')
  expect(searchParams({ ...blank, tags, session: FUNDA })).toBe('A search takes 100 sessions, MsgTypes and tags at most.')
})

test('a range over a day is refused without asking', async () => {
  serve({})
  show()
  type('Since (UTC)', '2026-10-08T09:00')
  type('Until (UTC; a day after since if empty)', '2026-10-09T10:00')
  await searchNow()
  expect(screen.getByRole('alert').textContent).toBe('A search covers 24 hours at most.')
  expect(document.querySelector('table.messages')).toBeNull()
})

test('the form asks with repeated tags, and pages on through the list', async () => {
  const first = `/api/messages?session=FIX.4.4%3AJETBRIDGE-%3EFUNDA&msg_type=D%2CF&tag=11%3DC7&tag=55%3DMSFT&${SINCE}&${UNTIL}`
  const pages: Page[] = [
    { messages: [hit('8=FIX.4.4|35=D|')], next: 'n1', follow: null },
    { messages: [hit('8=FIX.4.4|35=F|')], next: null, follow: null },
  ]
  const calls = serve({ [first]: () => json(pages[0]), [`${first}&after=n1`]: () => json(pages[1]) })
  show()
  type('Session', FUNDA)
  type('MsgType', 'D,F')
  type('Since (UTC)', '2026-10-08T09:00')
  type('Tag', '11')
  type('Value', 'C7')
  fireEvent.click(screen.getByRole('button', { name: 'Add tag condition' }))
  type('Tag', '55', 1)
  type('Value', 'MSFT', 1)
  await searchNow()
  expect(rawShown()).toEqual(['8=FIX.4.4|35=D|'])
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'More' })))
  expect(rawShown()).toEqual(['8=FIX.4.4|35=D|', '8=FIX.4.4|35=F|'])
  expect(screen.queryByRole('button', { name: 'More' })).toBeNull()

  // The same search again starts afresh.
  pages[0] = { messages: [hit('8=FIX.4.4|35=8|')], next: null, follow: null }
  await searchNow()
  expect(rawShown()).toEqual(['8=FIX.4.4|35=8|'])
  expect(calls).toEqual([first, `${first}&after=n1`, first])
})

test('the known sessions are suggested; a scoped user only its clients’', () => {
  const options = () => [...document.querySelectorAll('datalist option')].map((option) => option.getAttribute('value'))
  const { unmount } = show()
  expect(options()).toEqual([FUNDA, VENUE])
  unmount()
  show(who(['FUNDA']))
  expect(options()).toEqual([FUNDA])
})

test('a 403 is shown, and clears what was shown', async () => {
  const query = `/api/messages?session=FIX.4.4%3AJETBRIDGE-%3EFUNDB&${SINCE}&${UNTIL}`
  let answer = () => json({ error: 'u may see only clients FUNDA, not FIX.4.4:JETBRIDGE->FUNDB' }, 403)
  serve({ [query]: () => answer(), [`${query}&after=n1`]: () => answer() })
  show(who(['FUNDA']))
  type('Session', 'FIX.4.4:JETBRIDGE->FUNDB')
  type('Since (UTC)', '2026-10-08T09:00')
  await searchNow()
  expect(screen.getByRole('alert').textContent).toBe('u may see only clients FUNDA, not FIX.4.4:JETBRIDGE->FUNDB')

  // Rescoped between pages: what the first showed goes.
  answer = () => json({ messages: [hit('8=FIX.4.4|35=D|')], next: 'n1', follow: null })
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Retry' })))
  expect(rawShown()).toHaveLength(1)
  answer = () => json({ error: 'not yours' }, 403)
  await act(async () => fireEvent.click(screen.getByRole('button', { name: 'More' })))
  expect(screen.getByRole('alert').textContent).toBe('not yours')
  expect(rawShown()).toEqual([])
  expect(screen.queryByRole('button', { name: 'More' })).toBeNull()
})

test('a 400 shows the API’s text', async () => {
  const query = `/api/messages?${SINCE}&${UNTIL}`
  serve({ [query]: () => json({ error: 'a search covers 24 hours at most' }, 400) })
  show()
  type('Since (UTC)', '2026-10-08T09:00')
  await searchNow()
  expect(screen.getByRole('alert').textContent).toBe('a search covers 24 hours at most')
})
