export function capturedStandardOutput(result, capture) {
  if (capture !== true) return ''
  if (typeof result.stdout !== 'string') {
    throw new TypeError('captured child process stdout must be a string')
  }
  return result.stdout.trim()
}
