import { act, fireEvent, render, screen, within } from '@testing-library/react'
import { afterEach, expect, test, vi } from 'vitest'
import Risk from './Risk.tsx'
import { initial, LiveContext, reduce, type LiveState } from './store.ts'
import type { EditedView, Me, ReloadView, RiskTables, RiskView } from './types.ts'

const tables: RiskTables = {
  defaults: { max_order_qty: '1000', price_band: '5.5', symbols: ['AAPL', 'MSFT'], self_trade: true },
  clients: { FUNDA: { max_notional: '250000.50', self_trade: true }, FUNDB: {} },
}

const risk: RiskView = { defaults: {} as RiskView['defaults'], by_comp_id: {}, sessions: [], references: {}, blocked: [] }

const reload: ReloadView = {
  sessions: { added: ['FUNDC'], changed: [], removed: [], logged_out: [], started: [], reconfigured: [], stopped: [] },
  config: { routes: 2, clients: 2, day_end: '00:00 UTC' },
  warnings: ['FUNDC has no schedule'],
}

const snapshot = (clients: string[] | null = null): LiveState =>
  reduce(initial, { type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked: [], unavailable: false, clients })

const who = (role: Me['role'], clients: string[] | null = null): Me => ({ user: 'u', role, clients })

const page = (me: Me, state = snapshot(me.clients)) => (
  <LiveContext.Provider value={{ ...state, link: 'open' }}>
    <Risk me={me} reloads={0} />
  </LiveContext.Provider>
)

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status })

let stray: string[] = []

/** Answers fetches by `METHOD path` from `routes`, the risk and limits reads by default. */
function serve(routes: Record<string, () => Response> = {}) {
  const all: Record<string, () => Response> = { 'GET /api/risk': () => json(risk), 'GET /api/risk/limits': () => json(tables), ...routes }
  const calls: { key: string; init: RequestInit }[] = []
  vi.stubGlobal('fetch', async (url: string, init: RequestInit) => {
    const key = `${init.method} ${url}`
    calls.push({ key, init })
    const answer = all[key]
    if (answer === undefined) {
      stray.push(key)
      throw new Error(`unexpected fetch: ${key}`)
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

const section = () => document.querySelector('section.limits') as HTMLElement
const cells = (name: string) =>
  [...within(section()).getByRole('rowheader', { name }).closest('tr')!.querySelectorAll('th, td')].map((cell) => cell.textContent)
/** The section's buttons by their names: a row's name the table. */
const buttons = () =>
  within(section())
    .queryAllByRole('button')
    .map((b) => b.getAttribute('aria-label') ?? b.textContent)
const edits = (calls: { key: string; init: RequestInit }[]) =>
  calls.filter((c) => c.init.method !== 'GET').map((c) => [c.key, c.init.body])

/** Opens the dialog of the section's button named `name`, focused as a click would. */
async function open(name: string) {
  const button = within(section()).getByRole('button', { name })
  button.focus()
  await act(async () => fireEvent.click(button))
  return screen.getByRole('dialog')
}

/** Opens `button`'s dialog, types the reason and anything `fill` does, and confirms. */
async function actOn(button: string, reason: string, fill: (dialog: HTMLElement) => void = () => {}) {
  const dialog = await open(button)
  fireEvent.change(within(dialog).getByLabelText('Reason'), { target: { value: reason } })
  fill(dialog)
  await act(async () => fireEvent.click(within(dialog).getByRole('button', { name: /Confirm|Delete table/ })))
  return dialog
}

const pick = (dialog: HTMLElement, key: string) => fireEvent.change(within(dialog).getByLabelText('Key'), { target: { value: key } })
const type = (dialog: HTMLElement, label: RegExp, value: string) =>
  fireEvent.change(within(dialog).getByLabelText(label), { target: { value } })

test('shows the tables as written, and the keys a client leaves to the defaults', async () => {
  serve()
  render(page(who('viewer')))
  await within(section()).findByText('FUNDA')
  expect(cells('Defaults')).toEqual(['Defaults', '1000', '', 'AAPL, MSFT', '5.5', '', 'refused'])
  expect(cells('FUNDA')).toEqual(['FUNDA', 'default', '250000.50', 'default', 'default', 'default', 'refused'])
  expect(cells('FUNDB')).toEqual(['FUNDB', 'default', 'default', 'default', 'default', 'default', 'default'])
  expect(buttons()).toEqual([])
})

test('the buttons show by role and scope', async () => {
  serve()
  const { unmount } = render(page(who('operator')))
  await within(section()).findByText('FUNDA')
  expect(buttons()).toEqual([])
  unmount()

  render(page(who('admin')))
  await within(section()).findByText('FUNDA')
  expect(buttons()).toEqual(['Edit the defaults', 'Edit FUNDA', "Delete FUNDA's limits", 'Edit FUNDB', "Delete FUNDB's limits", 'Add…'])
})

test('a scoped admin may edit only its own tables, and add one', async () => {
  serve()
  render(page(who('admin', ['FUNDB'])))
  await within(section()).findByText('FUNDA')
  expect(buttons()).toEqual(['Edit FUNDB', "Delete FUNDB's limits", 'Add…'])
})

test('an edit sends the key and value, shows the table and the reload, and reads again', async () => {
  const answer: EditedView = { table: { max_notional: '250000.50', credit: '300.25', self_trade: true }, reload }
  const calls = serve({ 'PATCH /api/risk/limits/client/FUNDA': () => json(answer) })
  render(page(who('admin')))
  await within(section()).findByText('FUNDA')

  const dialog = await actOn('Edit FUNDA', 'more room', (dialog) => {
    expect(within(dialog).getByRole('heading').textContent).toBe('Edit FUNDA')
    const confirm = within(dialog).getByRole('button', { name: 'Confirm' }) as HTMLButtonElement
    expect(within(dialog).getAllByRole('option').map((o) => o.textContent)).toEqual([
      'Max order qty',
      'Max notional',
      'Symbols',
      'Price band %',
      'Credit',
      'Self trade',
    ])
    // A key the table doesn't set can't be removed.
    const remove = within(dialog).getByLabelText('Remove it: the default applies') as HTMLInputElement
    expect(remove.disabled).toBe(true)
    pick(dialog, 'max_notional')
    expect(remove.disabled).toBe(false)
    // Starts from the table's value, as written.
    expect((within(dialog).getByLabelText(/Value/) as HTMLInputElement).value).toBe('250000.50')
    pick(dialog, 'credit')
    expect(confirm.disabled).toBe(true)
    type(dialog, /Value/, '30O')
    expect(confirm.disabled).toBe(true)
    type(dialog, /Value/, ' 300.25 ')
    expect(confirm.disabled).toBe(false)
  })
  expect(dialog.isConnected).toBe(false)
  expect(edits(calls)).toEqual([['PATCH /api/risk/limits/client/FUNDA', '{"reason":"more room","set":{"credit":"300.25"}}']])
  expect(screen.getByRole('status').textContent).toBe('Set credit in FUNDA.')
  expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1 }))
  expect(section().querySelector('.answer')!.textContent).toBe(
    'FUNDA now sets max_notional 250000.50; credit 300.25; self_trade refused.' +
      'The reload took 2 routes, limits for 2 clients and day_end 00:00 UTC; sessions: added FUNDC.' +
      'FUNDC has no schedule',
  )
  // The limits read on opening and again after, and the risk after.
  expect(calls.filter((c) => c.key === 'GET /api/risk/limits')).toHaveLength(3)
  expect(calls.filter((c) => c.key === 'GET /api/risk')).toHaveLength(2)
})

test('symbols go as a list, self_trade as a boolean, and a new client by Add', async () => {
  const calls = serve({
    'PATCH /api/risk/limits/defaults': () => json({ table: {}, reload }),
    'PATCH /api/risk/limits/client/FUNDC': () => json({ table: {}, reload }),
  })
  render(page(who('admin')))
  await within(section()).findByText('FUNDA')
  await actOn('Edit the defaults', 'r', (dialog) => {
    pick(dialog, 'symbols')
    expect((within(dialog).getByLabelText(/Symbols/) as HTMLInputElement).value).toBe('AAPL, MSFT')
    type(dialog, /Symbols/, 'AAPL, MSFT IBM,')
  })
  fireEvent.change(within(section()).getByLabelText('Add client'), { target: { value: ' FUNDC ' } })
  const dialog = await open('Add…')
  expect(within(dialog).getByRole('heading').textContent).toBe('Edit FUNDC')
  fireEvent.change(within(dialog).getByLabelText('Reason'), { target: { value: 'r' } })
  pick(dialog, 'self_trade')
  // A client without its own starts from the defaults', and says so.
  const refuse = within(dialog).getByLabelText(/^Refuse self trades/) as HTMLInputElement
  expect(refuse.checked).toBe(true)
  expect(refuse.closest('label')!.textContent).toBe('Refuse self trades (default: refused)')
  // Inherited: nothing to send until the box is clicked, or the default would be pinned.
  const confirm = within(dialog).getByRole('button', { name: 'Confirm' }) as HTMLButtonElement
  expect(confirm.disabled).toBe(true)
  fireEvent.click(refuse)
  expect(confirm.disabled).toBe(false)
  await act(async () => fireEvent.click(within(dialog).getByRole('button', { name: 'Confirm' })))
  expect(edits(calls)).toEqual([
    ['PATCH /api/risk/limits/defaults', '{"reason":"r","set":{"symbols":["AAPL","MSFT","IBM"]}}'],
    ['PATCH /api/risk/limits/client/FUNDC', '{"reason":"r","set":{"self_trade":false}}'],
  ])
  expect(section().querySelector('.answer')!.textContent).toMatch(/^FUNDC sets no keys now\./)
})

test('removing a key sends null', async () => {
  const calls = serve({ 'PATCH /api/risk/limits/client/FUNDA': () => json({ table: {}, reload }) })
  render(page(who('admin')))
  await within(section()).findByText('FUNDA')
  await actOn('Edit FUNDA', 'r', (dialog) => {
    pick(dialog, 'self_trade')
    fireEvent.click(within(dialog).getByLabelText('Remove it: the default applies'))
    expect(within(dialog).queryByLabelText(/^Refuse self trades/)).toBeNull()
  })
  expect(edits(calls)).toEqual([['PATCH /api/risk/limits/client/FUNDA', '{"reason":"r","set":{"self_trade":null}}']])
  expect(screen.getByRole('status').textContent).toBe('Removed self_trade from FUNDA.')
})

test('a refusal stays in the dialog, saying why', async () => {
  const calls = serve({
    'PATCH /api/risk/limits/defaults': () => json({ error: 'risk.defaults.credit: -5 is not positive' }, 422),
  })
  render(page(who('admin')))
  await within(section()).findByText('FUNDA')
  const dialog = await actOn('Edit the defaults', 'r', (dialog) => {
    pick(dialog, 'credit')
    type(dialog, /Value/, '-5')
  })
  expect(within(dialog).getByRole('alert').textContent).toBe('risk.defaults.credit: -5 is not positive')
  expect(screen.getByRole('dialog')).toBe(dialog)
  expect(calls.filter((c) => c.key === 'GET /api/risk/limits')).toHaveLength(2)
})

test('Delete table deletes, and a scoped admin is shown no reload', async () => {
  const calls = serve({ 'DELETE /api/risk/limits/client/FUNDB': () => json({ table: null, reload: null }) })
  render(page(who('admin', ['FUNDB'])))
  await within(section()).findByText('FUNDB')
  await actOn("Delete FUNDB's limits", 'gone')
  expect(edits(calls)).toEqual([['DELETE /api/risk/limits/client/FUNDB', '{"reason":"gone"}']])
  expect(screen.getByRole('status').textContent).toBe("Deleted FUNDB's limits: its sessions have the defaults.")
  expect(section().querySelector('.answer')!.textContent).toBe('FUNDB has no table now.')
})

const again = (clients: string[] | null) =>
  reduce(snapshot(), { type: 'snapshot', sessions: [], orders: [], orders_next: null, blocked: [], unavailable: false, clients })

test('a 403 clears the tables and the edit open on them, and says why', async () => {
  let answer = () => json(tables)
  serve({ 'GET /api/risk/limits': () => answer() })
  const { rerender } = render(page(who('admin')))
  await within(section()).findByText('FUNDA')
  await open('Edit FUNDA')
  answer = () => json({ error: 'u may see only clients FUNDB' }, 403)
  // A snapshot reads again.
  await act(async () => rerender(page(who('admin'), again(['FUNDB']))))
  expect(within(section()).getByRole('alert').textContent).toBe('u may see only clients FUNDB')
  expect(section().querySelector('table')).toBeNull()
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(document.activeElement).toBe(screen.getByRole('heading', { level: 1 }))
})

test('a limits read answering after a newer one was asked is dropped', async () => {
  const answers: ((r: Response) => void)[] = []
  vi.stubGlobal('fetch', (url: string) =>
    url === '/api/risk/limits' ? new Promise<Response>((resolve) => answers.push(resolve)) : Promise.resolve(json(risk)),
  )
  const { rerender } = render(page(who('viewer')))
  rerender(page(who('viewer'), again(null)))
  expect(answers).toHaveLength(2)
  await act(async () => answers[1](json({ defaults: {}, clients: { FUNDB: {} } })))
  await act(async () => answers[0](json(tables)))
  expect(within(section()).queryByText('FUNDA')).toBeNull()
  expect(within(section()).getByText('FUNDB')).toBeTruthy()
})

test('Cancel closes the dialog, and focus goes back to its button', async () => {
  serve()
  render(page(who('admin')))
  await within(section()).findByText('FUNDA')
  const dialog = await open('Edit FUNDA')
  fireEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }))
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(document.activeElement).toBe(within(section()).getByRole('button', { name: 'Edit FUNDA' }))
})

test('a console reload reads the risk and the limits again', async () => {
  const calls = serve()
  const me = who('admin')
  const { rerender } = render(page(me))
  await within(section()).findByText('FUNDA')
  const reads = () => calls.map((c) => c.key).sort()
  expect(reads()).toEqual(['GET /api/risk', 'GET /api/risk/limits'])
  rerender(
    <LiveContext.Provider value={{ ...snapshot(), link: 'open' }}>
      <Risk me={me} reloads={1} />
    </LiveContext.Provider>,
  )
  await act(async () => {})
  expect(reads()).toEqual(['GET /api/risk', 'GET /api/risk', 'GET /api/risk/limits', 'GET /api/risk/limits'])
})
