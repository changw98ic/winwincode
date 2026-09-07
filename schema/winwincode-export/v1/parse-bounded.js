// SPDX-License-Identifier: Apache-2.0

/** Maximum byte length of one complete compact UTF-8 `winwincode-export/v1` document. */
export const MAX_WINWINCODE_EXPORT_BYTES = 16 * 1024 * 1024

const utf8Decoder = new TextDecoder('utf-8', { fatal: true })

/**
 * Applies the format's byte limit before decoding or schema validation.
 *
 * JSON Schema counts Unicode code points inside strings, so it cannot enforce a limit on the
 * serialized UTF-8 document. Every non-Rust consumer enters through this guard first.
 */
export function parseBoundedWinWinCodeExportJson(bytes) {
  if (!(bytes instanceof Uint8Array)) {
    throw new TypeError('WinWinCode export input must be a byte array')
  }
  if (bytes.byteLength > MAX_WINWINCODE_EXPORT_BYTES) {
    throw new RangeError('WinWinCode export exceeds its size limit')
  }
  return JSON.parse(utf8Decoder.decode(bytes))
}
