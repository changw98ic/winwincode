// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'

const schema = JSON.parse(
  await readFile(new URL('../schema/winwincode/v1/control-plane-http.schema.json', import.meta.url)),
)

const definition = schema.$defs.EnterprisePolicyDefinition
const payload = schema.$defs.EnterprisePolicyUpdatePayload
const projection = schema.$defs.EnterprisePolicyProjection
const reference = schema.$defs.EnterprisePolicyVersionReference

test('enterprise Policy contract freezes one version and inheritance authority model', () => {
  assert.deepEqual(definition.properties.childOverrideMode.enum, [
    'tighten_only',
    'allow_explicit_relaxation',
  ])
  assert.ok(definition.required.includes('childOverrideMode'))
  assert.deepEqual(payload.properties.inheritanceMode.enum, ['tighten', 'override'])
  for (const field of ['effectiveAt', 'inheritanceMode', 'baseVersion']) {
    assert.ok(payload.required.includes(field), `missing required update field ${field}`)
  }
  for (const field of [
    'scope',
    'source',
    'effectiveAt',
    'inheritanceMode',
    'baseVersion',
    'relaxationAuthority',
    'definitionSha256',
    'effectiveDefinitionSha256',
    'versionDigest',
    'version',
    'revision',
  ]) {
    assert.ok(projection.required.includes(field), `missing required projection field ${field}`)
  }
  assert.deepEqual(reference.required, [
    'policyId',
    'policyKind',
    'scope',
    'version',
    'definitionSha256',
    'effectiveDefinitionSha256',
    'versionDigest',
  ])
  assert.equal(projection.properties.scope.$ref, './domain.schema.json#/$defs/Scope')
  assert.equal(reference.properties.scope.$ref, './domain.schema.json#/$defs/Scope')
})
