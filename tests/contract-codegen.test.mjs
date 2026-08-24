import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const generator = join(root, 'scripts', 'generate-contracts.mjs')
const fixtureSchemas = join(root, 'tests', 'fixtures', 'contract-codegen', 'schema')

function runGenerator(outputRoot, ...extraArguments) {
  return spawnSync(process.execPath, [
    generator,
    '--schema-dir',
    fixtureSchemas,
    '--out-dir',
    outputRoot,
    ...extraArguments,
  ], {
    cwd: root,
    encoding: 'utf8',
  })
}

function generatedPaths(outputRoot) {
  return [
    join(outputRoot, 'rust', 'generated.rs'),
    join(outputRoot, 'typescript', 'generated.ts'),
    join(outputRoot, 'schema-collection.generated.json'),
    join(outputRoot, 'openapi.generated.json'),
    join(outputRoot, 'rust-domain', 'generated.rs'),
  ]
}

function commandFailure(result) {
  return [result.stdout, result.stderr, result.error?.stack].filter(Boolean).join('\n')
}

test('one canonical schema generation is deterministic and compiles for Rust and TypeScript', async t => {
  const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-codegen-'))
  t.after(() => rmSync(temporaryRoot, { force: true, recursive: true }))

  const first = runGenerator(temporaryRoot)
  assert.equal(first.status, 0, commandFailure(first))

  const paths = generatedPaths(temporaryRoot)
  const firstContents = paths.map(path => readFileSync(path, 'utf8'))
  const firstModifiedTimes = paths.map(path => statSync(path, { bigint: true }).mtimeNs)
  await new Promise(resolvePromise => setTimeout(resolvePromise, 25))

  const second = runGenerator(temporaryRoot)
  assert.equal(second.status, 0, commandFailure(second))
  assert.deepEqual(paths.map(path => readFileSync(path, 'utf8')), firstContents)
  assert.deepEqual(paths.map(path => statSync(path, { bigint: true }).mtimeNs), firstModifiedTimes)

  const typescript = firstContents[1]
  assert.match(typescript, /export type OrganizationId = string &/u)
  assert.match(typescript, /export enum ErrorCode/u)
  assert.match(typescript, /  InternalError = "INTERNAL_ERROR",/u)
  assert.match(typescript, /export type ContractEvent = CreatedEvent \| FailedEvent/u)
  assert.match(
    typescript,
    /export interface RecursiveJsonObject extends Readonly<Record<string, RecursiveJsonValue>> \{\}/u,
  )
  assert.match(
    typescript,
    /export type RecursiveJsonValue = null \| boolean \| number \| string \| ReadonlyArray<RecursiveJsonValue> \| RecursiveJsonObject/u,
  )
  assert.match(typescript, /export type SchemaVersion = "winwincode\/v1"/u)
  assert.match(firstContents[0], /pub enum SchemaVersion \{/u)
  assert.match(firstContents[0], /    WinwincodeV1,/u)
  assert.match(firstContents[0], /pub enum CreatedEventTypeValue \{/u)
  assert.match(firstContents[0], /    Created,/u)
  assert.match(firstContents[0], /pub type_value: CreatedEventTypeValue,/u)
  assert.match(
    typescript,
    /\/\*\*\n \* Read-only data shown by the fixture client\.\n \* It exercises multiline generated documentation\.\n \*\//u,
  )
  assert.doesNotMatch(typescript, /\n\/\*\* It exercises multiline/u)

  const schemaCollection = JSON.parse(firstContents[2])
  const openapi = JSON.parse(firstContents[3])
  assert.equal(
    schemaCollection.$defs.CreatedEvent.properties.organizationId.$ref,
    '#/$defs/OrganizationId',
  )
  assert.equal(
    openapi.paths['/fixtures/{organizationId}/widgets'].get.responses['200']
      .content['application/json'].schema.$ref,
    '#/components/schemas/WidgetProjection',
  )
  assert.deepEqual(Object.keys(openapi.components.schemas).sort(), Object.keys(schemaCollection.$defs).sort())

  const rustFixture = join(temporaryRoot, 'rust-fixture')
  mkdirSync(join(rustFixture, 'src'), { recursive: true })
  mkdirSync(join(rustFixture, 'domain', 'src'), { recursive: true })
  cpSync(paths[0], join(rustFixture, 'src', 'generated.rs'))
  cpSync(paths[4], join(rustFixture, 'domain', 'src', 'generated.rs'))
  writeFileSync(join(rustFixture, 'src', 'lib.rs'), 'pub mod generated;\n')
  writeFileSync(join(rustFixture, 'domain', 'src', 'lib.rs'), 'mod generated;\npub use generated::*;\n')
  writeFileSync(join(rustFixture, 'domain', 'Cargo.toml'), [
    '[package]',
    'name = "winwincode-domain"',
    'version = "0.0.0"',
    'edition = "2024"',
    'publish = false',
    '',
    '[dependencies]',
    'serde = { version = "=1.0.228", features = ["derive"] }',
    '',
  ].join('\n'))
  writeFileSync(join(rustFixture, 'Cargo.toml'), [
    '[package]',
    'name = "winwincode-contract-codegen-fixture"',
    'version = "0.0.0"',
    'edition = "2024"',
    'publish = false',
    '',
    '[workspace]',
    'members = ["domain"]',
    '',
    '[dependencies]',
    'serde = { version = "=1.0.228", features = ["derive"] }',
    'serde_json = "=1.0.149"',
    'winwincode-domain = { path = "domain" }',
    '',
  ].join('\n'))
  const cargo = spawnSync('cargo', [
    'check',
    '--offline',
    '--manifest-path',
    join(rustFixture, 'Cargo.toml'),
  ], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(cargo.status, 0, commandFailure(cargo))
  const rustfmt = spawnSync('cargo', [
    'fmt',
    '--all',
    '--manifest-path',
    join(rustFixture, 'Cargo.toml'),
    '--',
    '--check',
  ], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(rustfmt.status, 0, commandFailure(rustfmt))

  const tsc = spawnSync('corepack', [
    'pnpm',
    'exec',
    'tsc',
    '--ignoreConfig',
    '--pretty',
    'false',
    '--noEmit',
    '--strict',
    '--target',
    'ES2023',
    '--module',
    'ESNext',
    '--moduleResolution',
    'Bundler',
    paths[1],
  ], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(tsc.status, 0, commandFailure(tsc))
})

test('contract verification rejects a hand-edited generated artifact', t => {
  const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-codegen-drift-'))
  t.after(() => rmSync(temporaryRoot, { force: true, recursive: true }))

  const generated = runGenerator(temporaryRoot)
  assert.equal(generated.status, 0, commandFailure(generated))
  const typescriptPath = generatedPaths(temporaryRoot)[1]
  writeFileSync(typescriptPath, `${readFileSync(typescriptPath, 'utf8')}export type HandEdit = true\n`)

  const checked = runGenerator(temporaryRoot, '--check')
  assert.equal(checked.status, 1, commandFailure(checked))
  assert.match(checked.stderr, /generated contract drift: .*generated\.ts differs/u)

  const repaired = runGenerator(temporaryRoot)
  assert.equal(repaired.status, 0, commandFailure(repaired))
  const rechecked = runGenerator(temporaryRoot, '--check')
  assert.equal(rechecked.status, 0, commandFailure(rechecked))
})

test('checked-in contract artifacts carry Apache-2.0 release metadata and one canonical path', () => {
  const checked = spawnSync(process.execPath, [generator, '--check'], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(checked.status, 0, commandFailure(checked))

  const rustPath = join(root, 'crates', 'winwincode-api', 'src', 'generated.rs')
  const typescriptPath = join(root, 'apps', 'web', 'src', 'generated', 'contracts.ts')
  const domainRustPath = join(root, 'crates', 'winwincode-domain', 'src', 'generated.rs')
  const schemaCollectionPath = join(
    root,
    'schema',
    'winwincode',
    'v1',
    'schema-collection.generated.json',
  )
  const openapiPath = join(root, 'schema', 'winwincode', 'v1', 'openapi.generated.json')
  assert.match(readFileSync(rustPath, 'utf8'), /^\/\/ SPDX-License-Identifier: Apache-2\.0/u)
  assert.match(readFileSync(domainRustPath, 'utf8'), /^\/\/ SPDX-License-Identifier: Apache-2\.0/u)
  assert.match(readFileSync(typescriptPath, 'utf8'), /^\/\/ SPDX-License-Identifier: Apache-2\.0/u)

  const schemaCollection = JSON.parse(readFileSync(schemaCollectionPath, 'utf8'))
  const openapi = JSON.parse(readFileSync(openapiPath, 'utf8'))
  assert.equal(schemaCollection['x-winwincode-license'], 'Apache-2.0')
  assert.equal(openapi.info.license.identifier, 'Apache-2.0')

  const cargoManifest = readFileSync(
    join(root, 'crates', 'winwincode-api', 'Cargo.toml'),
    'utf8',
  )
  assert.match(cargoManifest, /license\.workspace = true/u)
  assert.match(cargoManifest, /publish = false/u)
  assert.match(cargoManifest, /include = \["src\/\*\*", "Cargo\.toml", "README\.md"\]/u)
  assert.equal(existsAtProductPath('packages/api-client'), false)
  assert.equal(existsAtProductPath('packages/contracts/src/generated'), false)
})

function existsAtProductPath(path) {
  return existsSync(join(root, path))
}
