import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

import Ajv2020 from 'ajv/dist/2020.js'
import addFormats from 'ajv-formats'

const root = resolve(import.meta.dirname, '..')
const schemaRoot = join(root, 'schema', 'winwincode', 'v1')
const schemaFiles = [
  'domain.schema.json',
  'control-plane-http.schema.json',
  'control-plane-events.schema.json',
  'execution-port.schema.json',
]

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function contractValidator() {
  const ajv = new Ajv2020({ allErrors: true, strict: true })
  addFormats(ajv)
  for (const [keyword, schemaType] of [
    ['x-authority', 'string'],
    ['x-direction', 'string'],
    ['x-winwincode-openapi', 'object'],
    ['x-winwincode-semantics', 'object'],
    ['x-winwincode-transports', 'object'],
  ]) ajv.addKeyword({ keyword, schemaType, valid: true })
  for (const name of schemaFiles) ajv.addSchema(json(join(schemaRoot, name)))
  return ajv
}

test('durable Server backpressure frame satisfies the canonical generated client schema', () => {
  const frame = json(join(
    root,
    'crates',
    'winwincode-server',
    'tests',
    'fixtures',
    'durable-event-hub.backpressure.valid.json',
  ))
  const events = json(join(schemaRoot, 'control-plane-events.schema.json'))
  const validate = contractValidator().getSchema(
    `${events.$id}#/$defs/ControlPlaneWebSocketServerFrame`,
  )
  assert.ok(validate, 'generated ControlPlaneWebSocketServerFrame validator is available')
  assert.equal(validate(frame), true, JSON.stringify(validate.errors))

  const nonContractFrame = structuredClone(frame)
  nonContractFrame.pendingEventCount = 1
  assert.equal(validate(nonContractFrame), false)
})
