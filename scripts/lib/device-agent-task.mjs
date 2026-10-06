import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'

export function loadDeviceAgentTask(path) {
  const bytes = readFileSync(path)
  assert.ok(bytes.length <= 4 * 1024 * 1024, 'task input exceeds 4 MiB')
  const task = JSON.parse(bytes.toString('utf8'))
  for (const field of ['title', 'goal', 'verificationCommand']) {
    assert.ok(typeof task[field] === 'string' && task[field].trim(), `${field} is required`)
  }
  for (const field of ['scope', 'constraints', 'outOfScope']) {
    assert.ok(Array.isArray(task[field]) && task[field].every(value => typeof value === 'string' && value.trim()),
      `${field} must contain strings`)
  }
  assert.ok(task.scope.length > 0, 'scope must not be empty')
  assert.ok(task.files && typeof task.files === 'object' && !Array.isArray(task.files), 'files must be an object')
  const files = Object.entries(task.files)
  assert.ok(files.length > 0 && files.length <= 100, 'task requires 1 to 100 input files')
  for (const [name, content] of files) {
    assert.ok(!name.includes('\\') && !name.includes('\0')
      && name.split('/').every(part => part && part !== '.' && part !== '..' && part.toLowerCase() !== '.git'),
    'input files must stay inside the task repository')
    assert.equal(typeof content, 'string', 'input files must contain text')
  }
  assert.ok(Array.isArray(task.acceptanceCriteria) && task.acceptanceCriteria.length > 0,
    'acceptanceCriteria must not be empty')
  const ids = new Set()
  for (const criterion of task.acceptanceCriteria) {
    assert.ok(criterion && typeof criterion.id === 'string' && criterion.id.trim()
      && typeof criterion.title === 'string' && criterion.title.trim() && criterion.required === true,
    'acceptance criteria require unique ids, titles and required=true')
    assert.ok(!ids.has(criterion.id), 'acceptance criterion ids must be unique')
    ids.add(criterion.id)
  }
  return { task, digest: createHash('sha256').update(bytes).digest('hex') }
}
