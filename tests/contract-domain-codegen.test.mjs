import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const generator = join(root, 'scripts', 'generate-contracts.mjs')
const fixtureSchemas = join(root, 'tests', 'fixtures', 'contract-codegen', 'schema')

function commandFailure(result) {
  return [result.stdout, result.stderr, result.error?.stack].filter(Boolean).join('\n')
}

test('Rust contract generation defines shared scalar value objects once in winwincode-domain', t => {
  const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-domain-codegen-'))
  t.after(() => rmSync(temporaryRoot, { force: true, recursive: true }))

  const generated = spawnSync(process.execPath, [
    generator,
    '--schema-dir',
    fixtureSchemas,
    '--out-dir',
    temporaryRoot,
  ], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(generated.status, 0, commandFailure(generated))

  const domainPath = join(temporaryRoot, 'rust-domain', 'generated.rs')
  const apiPath = join(temporaryRoot, 'rust', 'generated.rs')
  assert.equal(existsSync(domainPath), true, 'the generator must emit the shared Rust domain module')

  const domain = readFileSync(domainPath, 'utf8')
  const api = readFileSync(apiPath, 'utf8')
  for (const [name, primitive] of [
    ['OrganizationId', 'String'],
    ['RequestId', 'String'],
    ['Revision', 'i64'],
  ]) {
    assert.match(domain, new RegExp(`pub struct ${name}\\(pub ${primitive}\\);`, 'u'))
    const declarations = `${domain}\n${api}`.match(new RegExp(`pub (?:struct|type) ${name}\\b`, 'gu')) ?? []
    assert.equal(declarations.length, 1, `${name} must have one Rust declaration`)
  }
  assert.match(domain, /#\[serde\(transparent\)\]\npub struct OrganizationId\(pub String\);/u)
  assert.doesNotMatch(api, /pub (?:struct|type) (?:OrganizationId|RequestId|Revision)\b/u)
  assert.match(
    api,
    /pub organization_id: winwincode_domain::generated::OrganizationId,/u,
  )
  assert.match(api, /pub revision: winwincode_domain::generated::Revision,/u)

  const fixtureRoot = join(temporaryRoot, 'rust-workspace')
  mkdirSync(join(fixtureRoot, 'domain', 'src'), { recursive: true })
  mkdirSync(join(fixtureRoot, 'api', 'src'), { recursive: true })
  mkdirSync(join(fixtureRoot, 'api', 'tests'), { recursive: true })
  cpSync(domainPath, join(fixtureRoot, 'domain', 'src', 'generated.rs'))
  cpSync(apiPath, join(fixtureRoot, 'api', 'src', 'generated.rs'))
  writeFileSync(join(fixtureRoot, 'domain', 'src', 'lib.rs'), 'pub mod generated;\n')
  writeFileSync(join(fixtureRoot, 'api', 'src', 'lib.rs'), 'pub mod generated;\n')
  writeFileSync(join(fixtureRoot, 'Cargo.toml'), [
    '[workspace]',
    'members = ["api", "domain"]',
    'resolver = "2"',
    '',
  ].join('\n'))
  writeFileSync(join(fixtureRoot, 'domain', 'Cargo.toml'), [
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
  writeFileSync(join(fixtureRoot, 'api', 'Cargo.toml'), [
    '[package]',
    'name = "winwincode-api-fixture"',
    'version = "0.0.0"',
    'edition = "2024"',
    'publish = false',
    '',
    '[dependencies]',
    'serde = { version = "=1.0.228", features = ["derive"] }',
    'serde_json = "=1.0.149"',
    'winwincode-domain = { path = "../domain" }',
    '',
  ].join('\n'))
  writeFileSync(join(fixtureRoot, 'api', 'tests', 'wire.rs'), [
    'use winwincode_api_fixture::generated::WidgetProjection;',
    'use winwincode_domain::generated::{OrganizationId, Revision};',
    '',
    'fn accepts_domain_id(_: &OrganizationId) {}',
    'fn accepts_domain_revision(_: &Revision) {}',
    '',
    '#[test]',
    'fn api_dto_uses_domain_values_without_changing_json() {',
    '    let wire = serde_json::json!({',
    '        "id": "widget_1",',
    '        "labels": ["canonical"],',
    '        "organizationId": "org_fixture",',
    '        "revision": 7,',
    '        "schemaVersion": "winwincode/v1"',
    '    });',
    '    let projection: WidgetProjection = serde_json::from_value(wire.clone()).unwrap();',
    '    accepts_domain_id(&projection.organization_id);',
    '    accepts_domain_revision(&projection.revision);',
    '    assert_eq!(serde_json::to_value(projection).unwrap(), wire);',
    '}',
    '',
  ].join('\n'))

  const cargo = spawnSync('cargo', [
    'test',
    '--offline',
    '--manifest-path',
    join(fixtureRoot, 'Cargo.toml'),
  ], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(cargo.status, 0, commandFailure(cargo))
})

test('checked-in Rust contract crates expose domain values through the declared target graph', () => {
  const workspace = readFileSync(join(root, 'Cargo.toml'), 'utf8')
  const domainManifestPath = join(root, 'crates', 'winwincode-domain', 'Cargo.toml')
  const apiManifestPath = join(root, 'crates', 'winwincode-api', 'Cargo.toml')
  assert.equal(existsSync(domainManifestPath), true)
  assert.match(workspace, /"crates\/winwincode-domain"/u)

  const domainManifest = readFileSync(domainManifestPath, 'utf8')
  const apiManifest = readFileSync(apiManifestPath, 'utf8')
  const domainDependencies = domainManifest.match(/\[dependencies\]\n([\s\S]*?)(?:\n\[|$)/u)?.[1] ?? ''
  assert.doesNotMatch(domainDependencies, /winwincode-[a-z-]+/u)
  assert.match(apiManifest, /winwincode-domain\.workspace = true/u)

  const domainRust = readFileSync(
    join(root, 'crates', 'winwincode-domain', 'src', 'generated.rs'),
    'utf8',
  )
  const apiRust = readFileSync(
    join(root, 'crates', 'winwincode-api', 'src', 'generated.rs'),
    'utf8',
  )
  assert.doesNotMatch(apiRust, /pub use .*winwincode_domain/gu)

  const schemaDirectory = join(root, 'schema', 'winwincode', 'v1')
  const scalarDefinitions = readdirSync(schemaDirectory)
    .filter(name => name.endsWith('.schema.json'))
    .sort()
    .flatMap(name => {
      const schema = JSON.parse(readFileSync(join(schemaDirectory, name), 'utf8'))
      return Object.entries(schema.$defs ?? {})
    })
    .filter(([, schema]) => (
      ['boolean', 'integer', 'number', 'string'].includes(schema.type)
      && schema.const === undefined
      && schema.enum === undefined
      && schema.$ref === undefined
      && schema.allOf === undefined
      && schema.oneOf === undefined
      && schema.anyOf === undefined
    ))

  const idDefinitions = scalarDefinitions.filter(([name, schema]) => (
    name.endsWith('Id') && schema.type === 'string'
  ))
  assert.ok(idDefinitions.length >= 20, 'the gate must cover the full canonical ID set')

  const rustPrimitive = new Map([
    ['boolean', 'bool'],
    ['integer', 'i64'],
    ['number', 'f64'],
    ['string', 'String'],
  ])
  for (const [name, schema] of scalarDefinitions) {
    const primitive = rustPrimitive.get(schema.type)
    assert.match(
      domainRust,
      new RegExp(`pub struct ${name}\\(pub ${primitive}\\);`, 'u'),
      `${name} must be generated in winwincode-domain`,
    )
    const declarations = `${domainRust}\n${apiRust}`.match(
      new RegExp(`pub (?:struct|type) ${name}\\b`, 'gu'),
    ) ?? []
    assert.equal(declarations.length, 1, `${name} must have one Rust declaration`)
  }
})
