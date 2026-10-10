export const MAX_WINWINCODE_EXPORT_BYTES = 16 * 1024 * 1024

const utf8Decoder = new TextDecoder('utf-8', { fatal: true })

export function parseBoundedWinWinCodeExportJson(bytes) {
  if (!(bytes instanceof Uint8Array)) {
    throw new TypeError('WinWinCode export input must be a byte array')
  }
  if (bytes.byteLength > MAX_WINWINCODE_EXPORT_BYTES) {
    throw new RangeError('WinWinCode export exceeds its size limit')
  }
  return JSON.parse(utf8Decoder.decode(bytes))
}
