/**
 * An RFC 3339 time as the API writes it (UTC), shown to the second, or with `digits` of its
 * fraction: `2026-10-14 09:30:01`.
 */
export const utc = (time: string, digits = 0) => time.slice(0, digits > 0 ? 20 + digits : 19).replace('T', ' ')
