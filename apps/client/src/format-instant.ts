export function formatInstant(at: string): string {
  const moment = new Date(at)
  if (Number.isNaN(moment.getTime())) return at
  const pad = (value: number): string => String(value).padStart(2, '0')
  return `${String(moment.getFullYear())}-${pad(moment.getMonth() + 1)}-${pad(moment.getDate())} `
    + `${pad(moment.getHours())}:${pad(moment.getMinutes())}`
}
