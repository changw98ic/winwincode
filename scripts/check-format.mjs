#!/usr/bin/env node

import { readFileSync, readdirSync } from 'node:fs'
import { extname, join, relative, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const excludedDirectories = new Set([
  '.agents',
  '.beads',
  '.cache',
  '.claude',
  '.codex',
  '.git',
  'dist',
  'node_modules',
  'publication-secrets',
  'server-data',
  'source-repositories',
  'target',
  'third_party',
  'vendor',
])
const checkedExtensions = new Set(['.json', '.md', '.mjs', '.toml', '.ts', '.yaml', '.yml'])
const errors = []

function walk(directory) {
  const files = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (entry.isDirectory() && excludedDirectories.has(entry.name)) continue
    const path = join(directory, entry.name)
    if (entry.isDirectory()) files.push(...walk(path))
    else if (entry.isFile() && checkedExtensions.has(extname(entry.name))) files.push(path)
  }
  return files
}

for (const path of walk(root)) {
  const name = relative(root, path)
  const text = readFileSync(path, 'utf8')
  if (!text.endsWith('\n')) errors.push(`${name}: missing final newline`)
  if (text.includes('\r')) errors.push(`${name}: contains CR line endings`)
  if (text.includes('\t')) errors.push(`${name}: contains a tab character`)
  text.split('\n').forEach((line, index) => {
    if (/\s+$/u.test(line)) errors.push(`${name}:${index + 1}: trailing whitespace`)
  })
  if (extname(path) === '.json') {
    try {
      const canonical = `${JSON.stringify(JSON.parse(text), null, 2)}\n`
      if (text !== canonical) errors.push(`${name}: JSON is not canonical two-space formatting`)
    } catch (error) {
      errors.push(`${name}: invalid JSON: ${error.message}`)
    }
  }
}

if (errors.length > 0) {
  for (const error of errors) process.stderr.write(`${error}\n`)
  process.exit(1)
}

process.stdout.write('source formatting verified\n')
