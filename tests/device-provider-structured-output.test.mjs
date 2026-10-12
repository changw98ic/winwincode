// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import test from 'node:test'
import Ajv2020 from 'ajv/dist/2020.js'
import { build } from 'esbuild'

const schema = JSON.parse(readFileSync(resolve(import.meta.dirname, '../schema/winwincode/v1/device-provider.schema.json'), 'utf8'))
const validate = new Ajv2020({ strict: false }).compile({ ...schema, $ref: '#/$defs/DeviceProviderConfig' })
const bundle = await build({
  entryPoints: [resolve(import.meta.dirname, '../apps/client/src/generated/control-plane-client.ts')],
  bundle: true, write: false, platform: 'node', format: 'esm',
})
const { matchesCanonicalSchema } = await import(`data:text/javascript;base64,${Buffer.from(bundle.outputFiles[0].text).toString('base64')}`)

test('canonical and generated Device validators agree on Responses structured output modes', () => {
  for (const protocol of ['openai_responses', 'anthropic_messages', 'openai_chat_completions', 'canonical', 'codex_chatgpt', 'chatgpt_plan']) {
    for (const mode of [undefined, 'json_schema', 'json_object', 'text', null, 'unknown_mode', 1, {}]) {
      const config = {
        providerId: 'format-test', displayName: 'Format Test', endpoint: 'https://models.example/v1/responses',
        protocol, modelIds: ['test-model'], enabled: true,
        ...(mode === undefined ? {} : { responsesStructuredOutput: mode }),
      }
      const expected = mode === undefined || (protocol === 'openai_responses' && ['json_schema', 'json_object', 'text'].includes(mode))
      assert.equal(validate(config), expected, `canonical ${protocol}/${JSON.stringify(mode)}`)
      assert.equal(matchesCanonicalSchema('DeviceProviderConfig', config), expected, `generated ${protocol}/${JSON.stringify(mode)}`)
    }
  }
})
