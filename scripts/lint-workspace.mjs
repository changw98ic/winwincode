#!/usr/bin/env node

import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'

import { sourceBoundaryErrors } from './source-boundary-lint.mjs'

const root = resolve(import.meta.dirname, '..')
const errors = []
const workspacePackages = Object.freeze([
  'apps/client',
  'packages/contracts',
  'packages/strongflow',
])
const requiredIgnoredPaths = Object.freeze([
  '.cache/',
  'dist/',
  'node_modules/',
  'target/',
  '*.tgz',
])

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function sourceFiles(directory) {
  if (!existsSync(directory)) return []
  const result = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) result.push(...sourceFiles(path))
    else if (entry.isFile() && entry.name.endsWith('.ts')) result.push(path)
  }
  return result
}

const rootManifest = json(join(root, 'package.json'))
if (rootManifest.private !== true) errors.push('root package must remain private')
if (rootManifest.type !== 'module') errors.push('root package must use ESM')
if (rootManifest.license !== 'Apache-2.0') errors.push('root package license must be Apache-2.0')
if (rootManifest.packageManager !== 'pnpm@11.7.0') {
  errors.push('packageManager must be pnpm@11.7.0')
}
for (const [name, version] of Object.entries(rootManifest.devDependencies ?? {})) {
  if (typeof version !== 'string' || /^[~^*]|\bx\b|\|\||\s-\s/iu.test(version)) {
    errors.push(`dev dependency ${name} must use an exact version`)
  }
}

for (const packageDirectory of workspacePackages) {
  const manifestPath = join(root, packageDirectory, 'package.json')
  if (!existsSync(manifestPath)) {
    errors.push(`${packageDirectory}: missing package.json`)
    continue
  }
  const manifest = json(manifestPath)
  if (manifest.type !== 'module') errors.push(`${packageDirectory}: type must be module`)
  if (manifest.license !== 'Apache-2.0') {
    errors.push(`${packageDirectory}: license must be Apache-2.0`)
  }
  if (!Array.isArray(manifest.files) || manifest.files.length === 0) {
    errors.push(`${packageDirectory}: files allowlist is required`)
  }
  for (const [dependency, version] of Object.entries(manifest.dependencies ?? {})) {
    if (dependency.startsWith('@winwincode/') && version !== 'workspace:*') {
      errors.push(`${packageDirectory}: ${dependency} must use workspace:*`)
    }
  }
  for (const [dependency, version] of Object.entries(manifest.optionalDependencies ?? {})) {
    if (dependency.startsWith('@winwincode/') && version !== 'workspace:*') {
      errors.push(`${packageDirectory}: optional ${dependency} must use workspace:*`)
    }
  }
}

for (const packageDirectory of workspacePackages) {
  for (const path of sourceFiles(join(root, packageDirectory))) {
    const name = relative(root, path)
    const text = readFileSync(path, 'utf8')
    if (/\brequire\s*\(/u.test(text) || /\bmodule\.exports\b/u.test(text)) {
      errors.push(`${name}: CommonJS is forbidden`)
    }
    if (/from\s+['"][^'"]+\/src\//u.test(text)) {
      errors.push(`${name}: imports another package's private src path`)
    }
    if (/\bany\b/u.test(text)) errors.push(`${name}: explicit any is forbidden`)
    if (/\.(?:only|skip)\s*\(/u.test(text)) {
      errors.push(`${name}: focused or skipped test marker is forbidden`)
    }
  }
}

const ignore = readFileSync(join(root, '.gitignore'), 'utf8')
const ignoredPaths = new Set(ignore.split(/\r?\n/u).map(line => line.trim()))
for (const path of requiredIgnoredPaths) {
  if (!ignoredPaths.has(path)) errors.push(`.gitignore must contain ${path}`)
}

const sourceLock = json(join(root, 'upstream', 'sources.lock.json'))
const contractSource = readFileSync(join(root, 'packages', 'contracts', 'src', 'index.ts'), 'utf8')
for (const target of sourceLock.project.targets) {
  if (!contractSource.includes(`'${target}'`)) {
    errors.push(`contracts are missing release target ${target}`)
  }
}

errors.push(...sourceBoundaryErrors())

if (errors.length > 0) {
  for (const error of errors) process.stderr.write(`${error}\n`)
  process.exit(1)
}

process.stdout.write('workspace source and boundary lint verified\n')
