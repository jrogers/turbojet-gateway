import { cleanup } from '@testing-library/react'
import { afterEach } from 'vitest'

// Testing Library unmounts after each test only by itself under Vitest's `globals`, which is off.
afterEach(cleanup)
