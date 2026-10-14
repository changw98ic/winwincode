#!/usr/bin/env node

import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { dirname, extname, join, relative, resolve } from 'node:path'

const repositoryRoot = resolve(import.meta.dirname, '..')
const excludedDirectories = new Set([
  '.beads',
  '.cache',
  '.git',
  'beads',
  'build',
  'cache',
  'dist',
  'generated',
  'node_modules',
  'target',
  'third-party',
  'third_party',
  'upstream',
  'vendor',
])

function walkMarkdown(directory) {
  if (!existsSync(directory)) return []
  const files = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    if (entry.isDirectory() && excludedDirectories.has(entry.name)) continue
    const path = join(directory, entry.name)
    if (entry.isDirectory()) files.push(...walkMarkdown(path))
    else if (entry.isFile() && extname(entry.name) === '.md') files.push(path)
  }
  return files
}

function collectMarkdown(root) {
  const rootFiles = readdirSync(root, { withFileTypes: true })
    .filter(entry => entry.isFile() && extname(entry.name) === '.md')
    .map(entry => join(root, entry.name))
  return [...rootFiles, ...walkMarkdown(join(root, 'docs'))].toSorted()
}

function maskMarkdownCode(text) {
  const lines = text.split('\n')
  let fence
  return lines.map(line => {
    const marker = line.match(/^ {0,3}(`{3,}|~{3,})/u)?.[1]
    if (fence !== undefined) {
      const indent = line.length - line.trimStart().length
      const closing = line.trim()
      if (
        indent <= 3
        && closing.length >= fence.length
        && [...closing].every(character => character === fence.character)
      ) fence = undefined
      return ' '.repeat(line.length)
    }
    if (marker !== undefined) {
      fence = { character: marker[0], length: marker.length }
      return ' '.repeat(line.length)
    }
    const masked = line.split('')
    for (let index = 0; index < line.length;) {
      if (line[index] !== '`') {
        index += 1
        continue
      }
      let end = index
      while (line[end] === '`') end += 1
      const delimiter = line.slice(index, end)
      const close = line.indexOf(delimiter, end)
      if (close === -1) {
        index = end
        continue
      }
      masked.fill(' ', index, close + delimiter.length)
      index = close + delimiter.length
    }
    return masked.join('')
  })
}

function normalizeReference(label) {
  return label.trim().replace(/\s+/gu, ' ').toLowerCase()
}

function destination(value) {
  const trimmed = value.trim()
  if (trimmed.startsWith('<')) {
    const end = trimmed.indexOf('>')
    return end === -1 ? undefined : trimmed.slice(1, end)
  }
  return trimmed.match(/^\S+/u)?.[0]
}

function definitions(lines) {
  const values = new Map()
  lines.forEach((line, index) => {
    const match = line.match(/^ {0,3}\[([^\]]+)\]:\s*(.+)$/u)
    if (match === null) return
    const target = destination(match[2])
    const label = normalizeReference(match[1])
    if (target !== undefined && !values.has(label)) values.set(label, { target, line: index + 1 })
  })
  return values
}

function links(lines, references) {
  const found = []
  const pattern = /!?\[([^\]]*)\](?:\(\s*(<[^>\n]*>|[^)\s]+)(?:\s+(?:"[^"]*"|'[^']*'|\([^)]*\)))?\s*\)|\[([^\]]*)\])?/gu
  lines.forEach((line, index) => {
    if (/^ {0,3}\[[^\]]+\]:\s*/u.test(line)) return
    for (const match of line.matchAll(pattern)) {
      if (match[2] !== undefined) {
        found.push({ target: destination(match[2]), line: index + 1 })
        continue
      }
      const reference = normalizeReference(match[3] === undefined || match[3] === '' ? match[1] : match[3])
      const definition = references.get(reference)
      if (definition !== undefined) found.push({ target: definition.target, line: index + 1 })
    }
  })
  return found
}

function localPath(target) {
  if (target === undefined || target === '' || target.startsWith('#')) return undefined
  if (/^[A-Za-z][A-Za-z0-9+.-]*:/u.test(target) || target.startsWith('//')) return undefined
  const path = target.split('#', 1)[0]
  if (path === '') return undefined
  try {
    return decodeURIComponent(path).replaceAll('\\ ', ' ')
  } catch {
    return path
  }
}

function checkMarkdown(root) {
  const errors = []
  const files = collectMarkdown(root)
  for (const file of files) {
    const lines = maskMarkdownCode(readFileSync(file, 'utf8'))
    const references = definitions(lines)
    for (const link of links(lines, references)) {
      const target = localPath(link.target)
      if (target === undefined) continue
      const resolved = target.startsWith('/') ? resolve(root, `.${target}`) : resolve(dirname(file), target)
      if (!existsSync(resolved)) {
        errors.push(`${relative(root, file)}:${link.line}: missing local link target: ${link.target}`)
      }
    }
  }
  return { errors, files }
}

const root = process.argv[2] === undefined ? repositoryRoot : resolve(process.argv[2])
const { errors, files } = checkMarkdown(root)
if (errors.length > 0) {
  for (const error of errors) process.stderr.write(`${error}\n`)
  process.exit(1)
}
process.stdout.write(`documentation links verified (${files.length} file(s) scanned)\n`)
