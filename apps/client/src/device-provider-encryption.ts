// SPDX-License-Identifier: Apache-2.0

import type { DeviceProviderConfig, DeviceConfigurationEnvelope, DeviceProviderSnapshot } from './generated/contracts.js'

export interface DeviceProviderMutation {
  readonly operation: 'save' | 'delete' | 'test'
  readonly config: DeviceProviderConfig
  readonly apiKey?: string
}

export type DeviceExtensionMutation =
  | { readonly operation: 'save_skill'; readonly id: string; readonly content?: string; readonly sourcePath?: string; readonly enabled: boolean }
  | { readonly operation: 'save_mcp'; readonly id: string; readonly configuration: string; readonly enabled: boolean }
  | { readonly operation: 'set_enabled'; readonly kind: 'skill' | 'mcp'; readonly id: string; readonly enabled: boolean }
  | { readonly operation: 'delete'; readonly kind: 'skill' | 'mcp'; readonly id: string }
  | { readonly operation: 'test_mcp'; readonly id: string }

type ConfigurationSnapshot = Pick<DeviceProviderSnapshot, 'clientNodeId' | 'revision' | 'encryptionPublicKey'>

export function encryptDeviceProvider(snapshot: DeviceProviderSnapshot, requestId: string, mutation: DeviceProviderMutation, crypto: Crypto = globalThis.crypto): Promise<DeviceConfigurationEnvelope> {
  return encryptConfiguration('winwincode.device-provider.v1', snapshot, requestId, mutation, crypto)
}
export function encryptDeviceExtension(snapshot: ConfigurationSnapshot, requestId: string, mutation: DeviceExtensionMutation, crypto: Crypto = globalThis.crypto): Promise<DeviceConfigurationEnvelope> {
  return encryptConfiguration('winwincode.device-extensions.v1', snapshot, requestId, mutation, crypto)
}
const encode = (bytes: Uint8Array): string => btoa(String.fromCharCode(...bytes))
const decode = (value: string): Uint8Array<ArrayBuffer> => Uint8Array.from(atob(value), character => character.charCodeAt(0))

/** Encrypt the complete configuration for the selected Device before sending HTTP. */
async function encryptConfiguration(
  context: string,
  snapshot: ConfigurationSnapshot,
  requestId: string,
  mutation: DeviceProviderMutation | DeviceExtensionMutation,
  crypto: Crypto = globalThis.crypto,
): Promise<DeviceConfigurationEnvelope> {
  const encoder = new TextEncoder()
  const aad = encoder.encode(`${context}\n${snapshot.clientNodeId}\n${requestId}\n${snapshot.revision}`)
  const remote = await crypto.subtle.importKey('raw', decode(snapshot.encryptionPublicKey), { name: 'ECDH', namedCurve: 'P-256' }, false, [])
  const pair = await crypto.subtle.generateKey({ name: 'ECDH', namedCurve: 'P-256' }, true, ['deriveBits'])
  const shared = await crypto.subtle.deriveBits({ name: 'ECDH', public: remote }, pair.privateKey, 256)
  const material = await crypto.subtle.importKey('raw', shared, 'HKDF', false, ['deriveKey'])
  new Uint8Array(shared).fill(0)
  const key = await crypto.subtle.deriveKey({ name: 'HKDF', hash: 'SHA-256', salt: encoder.encode(context), info: aad }, material, { name: 'AES-GCM', length: 256 }, false, ['encrypt'])
  const nonce = crypto.getRandomValues(new Uint8Array(12))
  const plaintext = encoder.encode(JSON.stringify(mutation))
  try {
    const ciphertext = await crypto.subtle.encrypt({ name: 'AES-GCM', iv: nonce, additionalData: aad }, key, plaintext)
    return { clientNodeId: snapshot.clientNodeId, requestId, expectedRevision: snapshot.revision,
      publicKey: encode(new Uint8Array(await crypto.subtle.exportKey('raw', pair.publicKey))), nonce: encode(nonce), ciphertext: encode(new Uint8Array(ciphertext)) }
  } finally { plaintext.fill(0) }
}
