// SPDX-License-Identifier: Apache-2.0

import { parseControlPlaneServerUrl } from './control-plane-client.js'

const RUNTIME_CONFIG_KEY = '__WINWINCODE_CLIENT_CONFIG__'

export interface WinWinCodeClientRuntimeConfig {
  readonly serverUrl: string
}

/** Read deployment configuration without baking an environment address into browser assets. */
export function readWinWinCodeClientRuntimeConfig(
  source: unknown = Reflect.get(globalThis, RUNTIME_CONFIG_KEY),
): WinWinCodeClientRuntimeConfig {
  if (source === null || typeof source !== 'object' || Array.isArray(source)) {
    throw new TypeError(`${RUNTIME_CONFIG_KEY}.serverUrl must be configured at deployment time.`)
  }
  const location = parseControlPlaneServerUrl(Reflect.get(source, 'serverUrl'))
  return Object.freeze({ serverUrl: location.serverUrl })
}
