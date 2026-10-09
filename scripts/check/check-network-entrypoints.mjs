// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const root = resolve(import.meta.dirname, '../..')
const inventory = JSON.parse(readFileSync(new URL('../lib/network-entrypoints.json', import.meta.url)))
const excluded = new Set(['crates/winwincode-publication/src/github.rs', 'crates/winwincode-s3-artifact-adapter/src/lib.rs'])
const native = /(?:ureq::Agent::(?:config_builder|with_parts)|reqwest::Client::(?:builder|new)|TcpStream::connect|connect_async_tls_with_config\s*\(|httpsRequest\s*\(|\bfetch\s*\(|new\s+WebSocket\s*\()/gu

function files(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    if (['node_modules', 'dist', 'target', '.git', 'tests', 'fixtures', 'examples'].includes(entry.name)) return []
    const path = join(directory, entry.name)
    return entry.isDirectory() ? files(path) : /\.(?:rs|ts|mjs)$/u.test(path) ? [path] : []
  })
}

export function networkEntryErrors(sourceRoot = root) {
  const errors = []
  const registered = new Map(inventory.entries.map(entry => [entry.file, entry]))
  for (const entry of inventory.entries) {
    const source = readFileSync(join(sourceRoot, entry.file), 'utf8')
    for (const operation of entry.operations) {
      if (!source.includes(operation)) errors.push(`${entry.file}: missing registered operation ${operation}`)
    }
    if (!entry.policyMarkers.some(marker => source.replaceAll(/\s+/gu, '').includes(marker.replaceAll(/\s+/gu, '')))) errors.push(`${entry.file}: shared policy owner is missing`)
  }
  for (const directory of ['crates', 'apps', 'packages', 'scripts']) {
    for (const path of files(join(sourceRoot, directory))) {
      const name = relative(sourceRoot, path).replaceAll('\\', '/')
      if (excluded.has(name) || name === 'scripts/check/check-network-entrypoints.mjs' || /(?:test|spec)[._-]/u.test(name)) continue
      // Rust fixture modules do not participate in production ownership.
      const source = readFileSync(path, 'utf8').split(/#\[cfg\((?:test|all\(test,\s*unix\))\)\]\s*(?:#\[path[^\n]*\]\s*)?mod\s/u)[0]
      if ([...source.matchAll(native)].length > 0 && !registered.has(name)) errors.push(`${name}: unregistered native network entry`)
    }
  }
  return errors
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  assert.deepEqual(networkEntryErrors(), [])
  process.stdout.write(`network policy ownership verified (${inventory.entries.length} source boundaries)\n`)
}
