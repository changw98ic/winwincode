import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

import Ajv2020 from 'ajv/dist/2020.js'
import addFormats from 'ajv-formats'

const root = resolve(import.meta.dirname, '..')

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

test('prepared Rust Delivery ExecutionJob fixture satisfies the strict canonical schema', () => {
  const schemaRoot = join(root, 'schema', 'winwincode', 'v1')
  const domain = json(join(schemaRoot, 'domain.schema.json'))
  const execution = json(join(schemaRoot, 'execution-port.schema.json'))
  const fixture = json(join(
    root,
    'crates',
    'winwincode-control-plane',
    'tests',
    'fixtures',
    'prepared-delivery-execution-job.json',
  ))
  const ajv = new Ajv2020({ allErrors: true, strict: true })
  addFormats(ajv)
  for (const [keyword, schemaType] of [
    ['x-authority', 'string'],
    ['x-direction', 'string'],
    ['x-winwincode-semantics', 'object'],
    ['x-winwincode-transports', 'object'],
  ]) ajv.addKeyword({ keyword, schemaType, valid: true })
  ajv.addSchema(domain)
  ajv.addSchema(execution)
  const validate = ajv.getSchema(`${execution.$id}#/$defs/ExecutionJob`)

  assert.ok(validate)
  assert.equal(validate(fixture), true, JSON.stringify(validate.errors))
})
