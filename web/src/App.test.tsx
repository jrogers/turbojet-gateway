import { render, screen } from '@testing-library/react'
import { expect, test } from 'vitest'
import App from './App.tsx'

test('says Jetbridge', () => {
  render(<App />)
  expect(screen.getByRole('heading').textContent).toBe('Jetbridge')
})
