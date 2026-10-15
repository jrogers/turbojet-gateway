import type { SessionView } from './types.ts'

/**
 * A session's target, the client's side: what follows `->` in `BEGIN:SENDER->TARGET` (maybe with
 * `/SUB` and `:QUAL`), or `''` if it isn't a session's name.
 */
function target(session: string): string {
  const at = session.indexOf('->')
  return at < 0 ? '' : session.slice(at + 2)
}

/** A session's TargetCompID, the client's. */
export const targetCompId = (session: string) => target(session).split(/[/:]/)[0]

/**
 * `api::read::names_target`: whether `session` has `compId` as its TargetCompID. Not
 * `targetCompId(session) === compId`, so that a CompID holding `/` or `:` matches as the API's does.
 */
export function namesTarget(session: string, compId: string): boolean {
  const rest = target(session)
  if (rest === '' || !rest.startsWith(compId)) return false
  const after = rest.slice(compId.length)
  return after === '' || after[0] === '/' || after[0] === ':'
}

/**
 * Whether a session counts as down: disconnected and not paused, as a paused one is so on purpose,
 * and one logging on or out is on its way.
 */
export const isDown = (session: SessionView) => !session.paused && session.status === 'disconnected'
