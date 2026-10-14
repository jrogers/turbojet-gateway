import { useSyncExternalStore } from 'react'

/** The console's screens, by the page's hash: `#/orders`, `#/sessions`, `#/orders/{order_id}`. */
export type Route = { screen: 'orders' } | { screen: 'sessions' } | { screen: 'order'; orderId: string }

/** The route `hash` names; `#/orders` for anything else. */
export function parseRoute(hash: string): Route {
  const path = hash.replace(/^#/, '')
  if (path === '/sessions') return { screen: 'sessions' }
  const order = /^\/orders\/(.+)$/.exec(path)
  if (order) {
    try {
      return { screen: 'order', orderId: decodeURIComponent(order[1]) }
    } catch {
      // A malformed escape: no such order, so the default.
    }
  }
  return { screen: 'orders' }
}

/** The hash for an order's page. */
export const orderHref = (orderId: string) => `#/orders/${encodeURIComponent(orderId)}`

function subscribe(changed: () => void) {
  window.addEventListener('hashchange', changed)
  return () => window.removeEventListener('hashchange', changed)
}

const hash = () => window.location.hash

/** The current route, following the hash. */
export function useRoute(): Route {
  return parseRoute(useSyncExternalStore(subscribe, hash))
}
