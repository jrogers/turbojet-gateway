import type { Socket } from './live.ts'

/** A socket the test drives: what `connect` opened, in order. */
export class FakeSocket implements Socket {
  static opened: FakeSocket[] = []
  url: string
  closed = false
  onopen: Socket['onopen'] = null
  onmessage: Socket['onmessage'] = null
  onclose: Socket['onclose'] = null

  constructor(url: string) {
    this.url = url
    FakeSocket.opened.push(this)
  }

  close() {
    this.closed = true
  }

  open() {
    this.onopen?.call(this as unknown as WebSocket, new Event('open'))
  }

  send(message: unknown) {
    this.onmessage?.call(this as unknown as WebSocket, new MessageEvent('message', { data: JSON.stringify(message) }))
  }

  /** The server closing it with `code`. */
  end(code: number, reason = '') {
    this.onclose?.call(this as unknown as WebSocket, new CloseEvent('close', { code, reason }))
  }
}

/** Opens a [`FakeSocket`]. */
export const fake = (url: string) => new FakeSocket(url)
