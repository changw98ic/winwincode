#!/usr/bin/env node

import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const errors = []
const expectedWorkspacePackages = [
  'apps/host',
  'packages/contracts',
  'packages/dsh-profile',
  'packages/native',
  'packages/native-darwin-arm64',
  'packages/native-darwin-x64',
  'packages/native-linux-arm64',
  'packages/native-linux-x64',
  'packages/strongflow',
]
const requiredIgnoredPaths = [
  '.cache/',
  'dist/',
  'node_modules/',
  'packages/native-*/prebuild/',
  'target/',
  '*.tgz',
]

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function TypeScriptFiles(directory) {
  const result = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) result.push(...TypeScriptFiles(path))
    else if (entry.isFile() && entry.name.endsWith('.ts')) result.push(path)
  }
  return result
}

const rootPackage = json(join(root, 'package.json'))
if (rootPackage.private !== true) errors.push('root package must remain private')
if (rootPackage.type !== 'module') errors.push('root package must use ESM')
if (rootPackage.license !== 'Apache-2.0') errors.push('root package license must be Apache-2.0')
if (rootPackage.packageManager !== 'pnpm@11.7.0') errors.push('packageManager must be pnpm@11.7.0')
for (const [name, version] of Object.entries(rootPackage.devDependencies ?? {})) {
  if (/^[~^*]|\bx\b|\|\||\s-\s/iu.test(version)) errors.push(`dev dependency ${name} must use an exact version`)
}

for (const packageDirectory of expectedWorkspacePackages) {
  const manifestPath = join(root, packageDirectory, 'package.json')
  if (!existsSync(manifestPath)) {
    errors.push(`${packageDirectory}: missing package.json`)
    continue
  }
  const manifest = json(manifestPath)
  if (manifest.type !== 'module') errors.push(`${packageDirectory}: type must be module`)
  if (manifest.license !== 'Apache-2.0') errors.push(`${packageDirectory}: license must be Apache-2.0`)
  if (!Array.isArray(manifest.files) || manifest.files.length === 0) errors.push(`${packageDirectory}: files allowlist is required`)
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

for (const sourceRoot of ['apps', 'packages']) {
  for (const path of TypeScriptFiles(join(root, sourceRoot))) {
    const name = relative(root, path)
    const text = readFileSync(path, 'utf8')
    if (/\brequire\s*\(/u.test(text) || /\bmodule\.exports\b/u.test(text)) errors.push(`${name}: CommonJS is forbidden`)
    if (/from\s+['"][^'"]+\/src\//u.test(text)) errors.push(`${name}: imports another package's private src path`)
    if (/\bany\b/u.test(text)) errors.push(`${name}: explicit any is forbidden`)
    if (/\.(?:only|skip)\s*\(/u.test(text)) errors.push(`${name}: focused or skipped test marker is forbidden`)
  }
}

const ignore = readFileSync(join(root, '.gitignore'), 'utf8')
for (const path of requiredIgnoredPaths) {
  if (!ignore.split(/\r?\n/u).includes(path)) errors.push(`.gitignore must contain ${path}`)
}

const sourceLock = json(join(root, 'upstream', 'sources.lock.json'))
const contractSource = readFileSync(join(root, 'packages', 'contracts', 'src', 'index.ts'), 'utf8')
for (const target of sourceLock.project.targets) {
  if (!contractSource.includes(`'${target}'`)) errors.push(`contracts are missing release target ${target}`)
}

if (errors.length > 0) {
  for (const error of errors) process.stderr.write(`${error}\n`)
  process.exit(1)
}

process.stdout.write('workspace source lint verified\n')
