import { cleanup } from '@testing-library/react'
import { afterEach } from 'vitest'

// Testing Library unmounts after each test only by itself under Vitest's `globals`, which is off.
afterEach(cleanup)

// jsdom has <dialog> but not its modal methods: these open and close it, which is what the tests see.
HTMLDialogElement.prototype.showModal ??= function (this: HTMLDialogElement) {
  this.open = true
}
HTMLDialogElement.prototype.close ??= function (this: HTMLDialogElement) {
  this.open = false
}
