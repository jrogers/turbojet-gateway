import { expect, test } from 'vitest'
import { namesTarget, targetCompId } from './sessions.ts'

test('a session names its TargetCompID, past any SubID or qualifier', () => {
  expect(targetCompId('FIX.4.4:JETBRIDGE->FUNDA')).toBe('FUNDA')
  expect(targetCompId('FIX.4.4:JETBRIDGE->FUNDA/DESK1:Q')).toBe('FUNDA')
  expect(targetCompId('FIX.4.4:JETBRIDGE->FUNDA:Q')).toBe('FUNDA')
  expect(namesTarget('FIX.4.4:JETBRIDGE->FUNDA/DESK1', 'FUNDA')).toBe(true)
  expect(namesTarget('FIX.4.4:JETBRIDGE->FUNDA:Q', 'FUNDA')).toBe(true)
  expect(namesTarget('FIX.4.4:JETBRIDGE->FUNDAB', 'FUNDA')).toBe(false)
  expect(namesTarget('FIX.4.4:FUNDA->JETBRIDGE', 'FUNDA')).toBe(false)
  expect(namesTarget('FUNDA', 'FUNDA')).toBe(false)
})
