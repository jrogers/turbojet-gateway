/**
 * An RFC 3339 time as the API writes it (UTC), shown to the second, or with `digits` of its
 * fraction: `2026-10-14 09:30:01`.
 */
export const utc = (time: string, digits = 0) => time.slice(0, digits > 0 ? 20 + digits : 19).replace('T', ' ')

/** A `datetime-local` value (`2026-10-14T09:30`, maybe with seconds) as UTC, in ms; NaN if not one. */
export function utcTime(value: string): number {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?$/.test(value)) return NaN
  // With an explicit Z, never the browser's zone.
  return Date.parse(`${value}Z`)
}
