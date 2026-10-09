// SPDX-License-Identifier: Apache-2.0

// One contract for policy producers and the Device process boundary.
export const deviceAgentEnvironmentKeys = Object.freeze([
  'PYTHONDONTWRITEBYTECODE',
  'WWC_WORKER_APPROVAL_OWNER',
  'WWC_WORKER_MODEL_REASONING_EFFORT',
  'WWC_WORKER_FUSION',
  'WWC_WORKER_JEV_JUDGE',
  'WWC_WORKER_JEV_CONTEXT',
  'WWC_DEVICE_JEV_SETTINGS_FILE',
  'WWC_DEVICE_PROVIDER_HTTPS_PROXY',
  'WWC_BENCHMARK_TOOL_REPEAT_GUARD',
])

const fail = (code, field) => {
  throw Object.assign(new Error(field ? `${code}: ${field}` : code), { code, ...(field ? { field } : {}) })
}

export function deviceAgentEnvironment(environment, { strict = false } = {}) {
  if (strict) {
    for (const key of Object.keys(environment)) {
      if (!deviceAgentEnvironmentKeys.includes(key)) fail('DEVICE_AGENT_ENVIRONMENT_FIELD_UNDECLARED', key)
    }
  }
  const owner = environment.WWC_WORKER_APPROVAL_OWNER ?? 'core'
  if (!['core', 'execution_port'].includes(owner)) fail('DEVICE_AGENT_APPROVAL_OWNER_INVALID')
  return Object.fromEntries(deviceAgentEnvironmentKeys.map(key => [key,
    key === 'WWC_WORKER_APPROVAL_OWNER' ? owner
      : key === 'PYTHONDONTWRITEBYTECODE' ? environment[key] === '1' ? '1' : undefined
        : environment[key],
  ]))
}

export function assertDeviceAgentEnvironmentForwarded(expected, observed) {
  for (const key of deviceAgentEnvironmentKeys) {
    // Values can contain private paths or serialized policy. Report the field only.
    if (expected[key] !== observed[key]) fail('DEVICE_AGENT_POLICY_NOT_FORWARDED', key)
  }
}
