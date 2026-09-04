import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

// Phase 0 guard: the existing ExecutionPort contract must not absorb
// ClientControlPort (multi-user device control) messages. See
// docs/contracts/execution-port-v1.md and the Phase 0 plan task
// "freeze the existing ExecutionPort against device-control messages".
//
// This test intentionally uses plain regex/string parsing and recursive JSON
// walks so the boundary check stays dependency-free.

const root = resolve(import.meta.dirname, '..')
const contractDocPath = join(root, 'docs', 'contracts', 'execution-port-v1.md')
const executionPortSchemaPath = join(
  root,
  'schema',
  'winwincode',
  'v1',
  'execution-port.schema.json',
)
const clientControlSchemaPath = join(
  root,
  'schema',
  'winwincode',
  'v1',
  'client-control.schema.json',
)

// Device-control (ClientControlPort) vocabulary that must stay out of the
// ExecutionPort contract prose. Chosen to be stable and low-false-positive:
// the ExecutionPort legitimately uses lease/fencing/worker vocabulary, so
// those words are deliberately NOT in this list.
const deviceControlConceptKeywords = [
  { keyword: 'occupancy', concept: 'occupancy offer/lease/fence (ClientControlPort §9)' },
  { keyword: 'connect_code', concept: 'connect code publication (client.connect_code.published)' },
  { keyword: 'clientNodeId', concept: 'ClientControlPort envelope identity field' },
  { keyword: 'clientInstanceId', concept: 'ClientControlPort envelope identity field' },
  { keyword: 'enroll', concept: 'client.enroll / client.enrollment_accepted' },
]

function collectSchemaKindValues(node, out, source) {
  if (Array.isArray(node)) {
    for (const item of node) collectSchemaKindValues(item, out, source)
    return out
  }
  if (node === null || typeof node !== 'object') return out
  const kind = node.kind
  if (kind !== null && typeof kind === 'object' && !Array.isArray(kind)) {
    if (typeof kind.const === 'string') {
      out.push({ source, kind: kind.const })
    }
    if (Array.isArray(kind.enum)) {
      for (const value of kind.enum) {
        if (typeof value === 'string') out.push({ source, kind: value })
      }
    }
  }
  for (const value of Object.values(node)) {
    collectSchemaKindValues(value, out, source)
  }
  return out
}

function extractSchemaKindEntries(schemaPath) {
  const parsed = JSON.parse(readFileSync(schemaPath, 'utf8'))
  return collectSchemaKindValues(parsed, [], 'execution-port.schema.json')
}

// Contract-doc message kinds appear as dotted backtick tokens such as
// `worker.register` or `job.outcome`. Single-token scope kinds
// (`product-session`, `delivery-stage`) carry no dot and are not message
// kinds, so they are not captured here.
function extractDocKinds(markdown) {
  const kinds = new Set()
  const pattern = /`([a-z][a-z0-9_]*(?:\.[a-z0-9_]+)+)`/g
  for (const match of markdown.matchAll(pattern)) kinds.add(match[1])
  return [...kinds]
}

function findClientKinds(entries) {
  return entries.filter((entry) => entry.kind.startsWith('client.'))
}

function formatKindConflicts(conflicts) {
  return conflicts
    .map((entry) => `${entry.source}: "${entry.kind}"`)
    .sort()
    .join('; ')
}

function findConceptMentions(markdown, keyword) {
  const needle = keyword.toLowerCase()
  const hits = []
  const lines = markdown.split(/\r?\n/)
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index]
    if (line.toLowerCase().includes(needle)) {
      hits.push({ line: index + 1, text: line.trim() })
    }
  }
  return hits
}

const docMarkdown = readFileSync(contractDocPath, 'utf8')
const docKinds = extractDocKinds(docMarkdown)
const schemaKindEntries = extractSchemaKindEntries(executionPortSchemaPath)
const schemaKinds = schemaKindEntries.map((entry) => entry.kind)

assert.ok(
  schemaKinds.length > 0,
  'execution-port.schema.json must expose at least one kind constant',
)
assert.ok(
  docKinds.length > 0,
  'execution-port-v1.md must reference at least one dotted message kind',
)

test('execution-port schema kind surface contains no client.* device-control kinds', () => {
  const conflicts = findClientKinds(schemaKindEntries)
  assert.deepEqual(
    conflicts,
    [],
    `ExecutionPort schema must not register ClientControlPort (device-control) message kinds, found: ${formatKindConflicts(conflicts)}`,
  )
})

test('execution-port contract doc kind surface contains no client.* device-control kinds', () => {
  const conflicts = docKinds
    .filter((kind) => kind.startsWith('client.'))
    .map((kind) => ({ source: 'execution-port-v1.md', kind }))
  assert.deepEqual(
    conflicts,
    [],
    `ExecutionPort contract doc must not reference ClientControlPort (device-control) message kinds, found: ${formatKindConflicts(conflicts)}`,
  )
})

test('client-control schema kind set is disjoint from execution-port kind set', (t) => {
  if (!existsSync(clientControlSchemaPath)) {
    t.skip('client-control.schema.json does not exist yet (parallel schema lane in progress)')
    return
  }
  const parsed = JSON.parse(readFileSync(clientControlSchemaPath, 'utf8'))
  const clientKindEntries = collectSchemaKindValues(parsed, [], 'client-control.schema.json')
  const executionKindSet = new Set(schemaKinds)
  const overlap = [
    ...new Set(
      clientKindEntries
        .map((entry) => entry.kind)
        .filter((kind) => executionKindSet.has(kind)),
    ),
  ].sort()
  assert.deepEqual(
    overlap,
    [],
    `ClientControlPort kinds must not overlap ExecutionPort kinds, shared kinds: ${overlap.map((kind) => `"${kind}"`).join(', ')}`,
  )
})

test('execution-port contract doc does not mention device-control concepts', () => {
  const conflicts = []
  for (const { keyword, concept } of deviceControlConceptKeywords) {
    for (const hit of findConceptMentions(docMarkdown, keyword)) {
      conflicts.push(`${hit.line}: keyword "${keyword}" (${concept}) in "${hit.text}"`)
    }
  }
  assert.deepEqual(
    conflicts,
    [],
    `ExecutionPort contract doc must not reference ClientControlPort device-control concepts, found:\n${conflicts.join('\n')}`,
  )
})
