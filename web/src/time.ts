/**
 * An RFC 3339 time as the API writes it (UTC), shown to the second, or with `digits` of its
 * fraction, padded with zeros where it has fewer: `2026-10-14 09:30:01`, `2026-10-14 09:30:01.200`.
 */
export function utc(time: string, digits = 0): string {
  const seconds = time.slice(0, 19).replace('T', ' ')
  if (digits === 0) return seconds
  // The API leaves out a zero fraction, and writes as many digits as it has.
  const fraction = /^\.(\d+)/.exec(time.slice(19))?.[1] ?? ''
  return `${seconds}.${fraction.slice(0, digits).padEnd(digits, '0')}`
}

/** A `datetime-local` value (`2026-10-14T09:30`, maybe with seconds) as UTC, in ms; NaN if not one. */
export function utcTime(value: string): number {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?$/.test(value)) return NaN
  // With an explicit Z, never the browser's zone.
  return Date.parse(`${value}Z`)
}
