#!/usr/bin/env node

import { readFileSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'

import { PRODUCT_PACKAGE_DIRECTORIES } from './release-source-contract.mjs'

const numericIdentifier = '(?:0|[1-9]\\d*)'
const alphanumericIdentifier = '(?:\\d*[A-Za-z-][0-9A-Za-z-]*)'
const prereleaseIdentifier = `(?:${numericIdentifier}|${alphanumericIdentifier})`
const semanticVersionPattern = new RegExp(
  `^${numericIdentifier}\\.${numericIdentifier}\\.${numericIdentifier}`
  + `(?:-${prereleaseIdentifier}(?:\\.${prereleaseIdentifier})*)?`
  + '(?:\\+[0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*)?$',
  'u',
)

function readManifest(path) {
  const manifest = JSON.parse(readFileSync(path, 'utf8'))
  if (typeof manifest !== 'object' || manifest === null || Array.isArray(manifest)) {
    throw new Error(`${path}: package manifest must be an object`)
  }
  return manifest
}

export function assertProductVersion(version) {
  if (typeof version !== 'string' || !semanticVersionPattern.test(version)) {
    throw new Error(`invalid semantic version: ${String(version)}`)
  }
}

export function setProductVersion(root, version) {
  assertProductVersion(version)
  const paths = [
    resolve(root, 'package.json'),
    ...PRODUCT_PACKAGE_DIRECTORIES.map(directory => resolve(root, directory, 'package.json')),
  ]
  const updates = paths.map(path => {
    const manifest = readManifest(path)
    return Object.freeze({
      path,
      text: `${JSON.stringify({ ...manifest, version }, null, 2)}\n`,
    })
  })
  for (const update of updates) writeFileSync(update.path, update.text)
  return Object.freeze(updates.map(update => update.path))
}

if (process.argv[1] !== undefined && resolve(process.argv[1]) === resolve(import.meta.filename)) {
  const [version, ...unexpected] = process.argv.slice(2)
  if (version === undefined || unexpected.length > 0) {
    process.stderr.write('Usage: corepack pnpm version:set VERSION\n')
    process.exitCode = 2
  } else {
    try {
      const updated = setProductVersion(resolve(import.meta.dirname, '..'), version)
      process.stdout.write(`set ${String(updated.length)} product manifests to ${version}\n`)
    } catch (error) {
      process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`)
      process.exitCode = 1
    }
  }
}
