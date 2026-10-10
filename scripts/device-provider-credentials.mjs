// SPDX-License-Identifier: Apache-2.0

/** Admit resolved credentials before contacting a Device or saving secrets. */
export function assertResolvedDeviceProviderCredentials({ apiKey, customHeaders }) {
  if (typeof apiKey !== 'string' || apiKey.trim().length === 0) {
    throw credentialError('PROVIDER_CREDENTIAL_MISSING')
  }
  const values = [apiKey, ...Object.values(customHeaders ?? {})]
  if (values.some(value => typeof value === 'string' && /\{(?:env|file):/u.test(value))) {
    throw credentialError('PROVIDER_CREDENTIAL_REFERENCE_UNRESOLVED')
  }
}

function credentialError(code) {
  return Object.assign(new Error(code), { code })
}
