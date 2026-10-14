import { expect, test } from 'vitest'
import { utc } from './time.ts'

test('utc shows the digits asked for, padded, and never the Z', () => {
  expect(utc('2026-10-14T09:30:01.123456Z')).toBe('2026-10-14 09:30:01')
  expect(utc('2026-10-14T09:30:01.123456Z', 3)).toBe('2026-10-14 09:30:01.123')
  expect(utc('2026-10-14T09:30:01Z', 3)).toBe('2026-10-14 09:30:01.000')
  expect(utc('2026-10-14T09:30:01.2Z', 3)).toBe('2026-10-14 09:30:01.200')
  expect(utc('2026-10-14T09:30:01+00:00', 2)).toBe('2026-10-14 09:30:01.00')
})
