#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { cpSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, join, relative, resolve, sep } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-clean-'))
const excludedNames = new Set([
  '.agents',
  '.beads',
  '.cache',
  '.claude',
  '.codex',
  '.git',
  'dist',
  'node_modules',
  'prebuild',
  'prebuilds',
  'target',
])
const commands = [
  ['install', '--frozen-lockfile', '--prefer-offline'],
  ['format:check'],
  ['lint'],
  ['test'],
  ['build'],
  ['verify:packages'],
  ['verify:upstream'],
]

try {
  cpSync(root, temporaryRoot, {
    recursive: true,
    filter(source) {
      const name = basename(source)
      if (source !== root && excludedNames.has(name)) return false
      const path = relative(root, source)
      return path === '' || !path.split(sep).some(segment => excludedNames.has(segment))
    },
  })

  for (const args of commands) {
    const result = spawnSync('corepack', ['pnpm', ...args], {
      cwd: temporaryRoot,
      encoding: 'utf8',
      env: {
        ...process.env,
        CI: '1',
        COREPACK_ENABLE_DOWNLOAD_PROMPT: '0',
      },
      timeout: 300_000,
    })
    if (result.status !== 0) {
      process.stderr.write(`clean checkout command failed: corepack pnpm ${args.join(' ')}\n`)
      process.stderr.write(result.stdout)
      process.stderr.write(result.stderr)
      process.exitCode = 1
      break
    }
    process.stdout.write(`clean checkout passed: pnpm ${args.join(' ')}\n`)
  }
} finally {
  rmSync(temporaryRoot, { force: true, recursive: true })
}
