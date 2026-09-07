#!/usr/bin/env node

import { rmSync } from 'node:fs'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dirname, '..')

for (const path of [
  'apps/client/dist',
  'packages/browser-core/dist',
  'packages/browser-ui/dist',
  'packages/contracts/dist',
  'packages/control-plane-client/dist',
  'packages/strongflow/dist',
  '.cache',
  'target',
]) {
  rmSync(join(root, path), { force: true, recursive: true })
}
