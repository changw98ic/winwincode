#!/usr/bin/env node

import { existsSync, readdirSync, rmSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')
const packageRoots = ['apps', 'packages']

for (const packageRoot of packageRoots) {
  const absoluteRoot = join(root, packageRoot)
  if (!existsSync(absoluteRoot)) continue
  for (const entry of readdirSync(absoluteRoot, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue
    rmSync(join(absoluteRoot, entry.name, 'dist'), { force: true, recursive: true })
  }
}

for (const path of ['target', '.cache']) {
  rmSync(join(root, path), { force: true, recursive: true })
}

rmSync(join(root, 'packages', 'native', 'prebuilds'), { force: true, recursive: true })
for (const packageName of [
  'native-darwin-arm64',
  'native-darwin-x64',
  'native-linux-arm64',
  'native-linux-x64',
]) {
  rmSync(join(root, 'packages', packageName, 'prebuild'), { force: true, recursive: true })
}
