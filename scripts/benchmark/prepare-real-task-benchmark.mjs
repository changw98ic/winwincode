// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { validateFrozenTaskSource } from './run-real-task-benchmark.mjs'

const [input, output] = process.argv.slice(2)
assert.ok(input && output, 'requires frozen task checkout and a new output directory')
const root = resolve(input), directory = resolve(output)
const source = await validateFrozenTaskSource({ repositoryRoot: root,
  repositoryUrl: 'https://github.com/changw9813/agent-benchmark-tasks',
  revision: 'fa9da301e493fb88d48c86cb8954ed46d9cd2ffe' })
mkdirSync(directory, { recursive: true, mode: 0o700 })
const catalog = JSON.parse(readFileSync(join(root, 'catalog.json')))
const environments = JSON.parse(readFileSync(join(root, 'environments/manifest.json')))
const images = new Map()
for (const environment of Object.values(environments)) {
  const imageId = execFileSync('docker', ['image', 'inspect', '--format', '{{.Id}}', environment.image], { encoding: 'utf8' }).trim()
  assert.match(imageId, /^sha256:[0-9a-f]{64}$/u)
  images.set(environment.image, imageId)
}
const tasks = []
for (const spec of catalog) {
  const task = join(root, 'tasks', spec.id)
  assert.deepEqual(JSON.parse(readFileSync(join(task, 'task.json'))), spec)
  const files = { 'TASK.md': readFileSync(join(task, 'task.md'), 'utf8'),
    'PROTOCOL.md': readFileSync(join(root, 'PROTOCOL.md'), 'utf8') }
  const walk = (path, prefix = '') => {
    for (const entry of readdirSync(path, { withFileTypes: true })) {
      const name = `${prefix}${entry.name}`
      if (entry.isDirectory()) walk(join(path, entry.name), `${name}/`)
      else {
        assert.ok(entry.isFile(), 'starter must contain regular source files')
        files[name] = readFileSync(join(path, entry.name), 'utf8')
      }
    }
  }
  walk(join(task, 'starter'))
  const imageId = images.get(environments[spec.environment].image)
  const bytes = `${JSON.stringify({ taskId: spec.id, spec, files, sourceRevision: source.revision,
    sourceDigest: source.sourceDigest, imageId }, null, 2)}\n`
  writeFileSync(join(directory, `${spec.id}.json`), bytes, { flag: 'wx', mode: 0o600 })
  tasks.push({ taskId: spec.id, sha256: createHash('sha256').update(bytes).digest('hex'), imageId })
}
writeFileSync(join(directory, 'prepared-inputs.json'), `${JSON.stringify({ source, tasks }, null, 2)}\n`,
  { flag: 'wx', mode: 0o600 })
console.log(JSON.stringify({ taskCount: tasks.length, source, directory }))
