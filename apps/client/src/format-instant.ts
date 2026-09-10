// SPDX-License-Identifier: Apache-2.0

/**
 * Human-facing rendering of one wire Instant. Wire timestamps are ISO-8601
 * with milliseconds and a UTC designator; users get the local
 * `YYYY-MM-DD HH:mm` form instead. A parse failure falls back to the
 * untouched wire value rather than inventing a date.
 */
export function formatInstant(at: string): string {
  const moment = new Date(at)
  if (Number.isNaN(moment.getTime())) return at
  const pad = (value: number): string => String(value).padStart(2, '0')
  return `${String(moment.getFullYear())}-${pad(moment.getMonth() + 1)}-${pad(moment.getDate())} `
    + `${pad(moment.getHours())}:${pad(moment.getMinutes())}`
}
