import assert from 'node:assert/strict'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const inventory = JSON.parse(readFileSync(join(root, 'docs/decisions/0032-community-persistence-ports.inventory.json'), 'utf8'))
const source = path => readFileSync(join(root, path), 'utf8')

function filesBelow(path) {
  if (!existsSync(path)) return []
  return readdirSync(path, { withFileTypes: true }).flatMap(entry => {
    const child = join(path, entry.name)
    return entry.isDirectory() ? filesBelow(child) : [child]
  })
}

function ownerSourceFiles() {
  const files = []
  for (const crate of inventory.scope.ownerCrates) {
    files.push(...filesBelow(join(root, 'crates', crate, 'src')).filter(path => path.endsWith('.rs')))
  }
  return files.map(path => relative(root, path)).sort()
}

function traitBlock(text, name) {
  const start = text.search(new RegExp(`^[ \\t]*pub trait\\s+${name}\\b`, 'm'))
  assert.notEqual(start, -1, `public trait is missing: ${name}`)
  const body = text.indexOf('{', start)
  let depth = 0
  for (let i = body; i < text.length; i += 1) {
    if (text[i] === '{') depth += 1
    if (text[i] === '}' && --depth === 0) return text.slice(start, i + 1)
  }
  assert.fail(`trait block is not closed: ${name}`)
}

function discoveredPorts() {
  const ports = []
  for (const crate of inventory.scope.ownerCrates) {
    const sourceRoot = join(root, 'crates', crate, 'src')
    for (const path of filesBelow(sourceRoot).filter(path => path.endsWith('.rs'))) {
      const text = readFileSync(path, 'utf8')
      const marked = new Set(
        [...text.matchAll(/^[ \t]*#\[doc = "winwincode-community-persistence-port"\][ \t]*\n[ \t]*pub trait\s+(\w+)/gm)].map(match => match[1]),
      )
      for (const match of text.matchAll(/^[ \t]*pub trait\s+(\w+)/gm)) {
        if (match[1].includes('Persistence') || marked.has(match[1])) {
          ports.push({ crate, trait: match[1], source: relative(root, path) })
        }
      }
    }
  }
  return ports.sort((a, b) => `${a.crate}/${a.trait}`.localeCompare(`${b.crate}/${b.trait}`))
}

function resultErrors(block) {
  let text = block.replace(/\/\/.*$/gm, '').replace(/\/\*[\s\S]*?\*\//g, '')
  const errors = new Set()
  let position = 0
  while (position < text.length) {
    const start = text.indexOf('Result<', position)
    if (start < 0) break
    let index = start + 7
    let angleDepth = 1
    while (index < text.length && angleDepth > 0) {
      if (text[index] === '<') angleDepth += 1
      if (text[index] === '>') angleDepth -= 1
      index += 1
    }
    const args = text.slice(start + 7, index - 1)
    let nestedDepth = 0
    let separator = -1
    for (let i = 0; i < args.length; i += 1) {
      if ('<([{'.includes(args[i])) nestedDepth += 1
      else if ('>)]}'.includes(args[i])) nestedDepth -= 1
      else if (args[i] === ',' && nestedDepth === 0) { separator = i; break }
    }
    if (separator >= 0) errors.add(args.slice(separator + 1).trim().replace(/,$/, '').trim())
    position = index
  }
  return [...errors].sort()
}

// Public export surface of one owner crate: bare `pub use` names plus the
// public items declared in the crate root. Glob re-exports would hide names,
// so they are rejected instead of parsed.
const exportNameCache = new Map()
function crateExportNames(crate) {
  if (exportNameCache.has(crate)) return exportNameCache.get(crate)
  const text = source(join('crates', crate, 'src/lib.rs'))
  assert.doesNotMatch(text, /\bpub use\s+[^;]*::\*/u, `${crate} re-exports through a glob, which hides exported names`)
  const names = new Set()
  for (const statement of text.matchAll(/\bpub use\s+([^;]+);/g)) {
    for (const raw of statement[1].split(/[{}]/).flatMap(part => part.split(','))) {
      const item = raw.trim()
      if (item === '') continue
      const name = item.split(' as ').pop().trim().split('::').pop().trim()
      if (/^[A-Z]/u.test(name)) names.add(name)
    }
  }
  for (const match of text.matchAll(/^pub (?:trait|struct|enum|fn|type|const|static)\s+(\w+)/gm)) names.add(match[1])
  const sorted = [...names].sort()
  exportNameCache.set(crate, sorted)
  return sorted
}

function productPersistenceExportPattern() {
  return new RegExp(`\\b(?:${inventory.contract.forbiddenProductWords.join('|')})[A-Za-z0-9_]*Persistence\\b`, 'iu')
}

test('Community inventory matches the public persistence ports', () => {
  assert.equal(inventory.schemaVersion, 1)
  assert.equal(inventory.task, 'winwincode-edition.2.4.1.2.2')
  assert.equal(inventory.scope.portCount, inventory.ports.length)
  assert.deepEqual(discoveredPorts(), inventory.ports.map(({ crate, trait, source }) => ({ crate, trait, source })))
  for (const entry of inventory.ports) {
    assert.ok(existsSync(join(root, entry.source)), `port source is missing: ${entry.source}`)
    assert.deepEqual(resultErrors(traitBlock(source(entry.source), entry.trait)), entry.stableErrors)
    assert.ok(
      crateExportNames(entry.crate).includes(entry.trait),
      `${entry.trait} is absent from the ${entry.crate} public export surface`,
    )
  }
})

test('public persistence ports reject product-specific storage types', () => {
  const forbidden = inventory.contract.forbiddenPublicTypePatterns
  for (const word of inventory.contract.forbiddenProductWords) {
    assert.ok(forbidden.includes(word), `forbiddenPublicTypePatterns must keep the product word ${word}`)
  }
  for (const entry of discoveredPorts()) {
    const block = traitBlock(source(entry.source), entry.trait)
    for (const pattern of forbidden) {
      assert.doesNotMatch(block, new RegExp(`\\b${pattern}\\b`, 'iu'), `${entry.trait} exposes ${pattern}`)
    }
  }
})

test('owner crate exports stay free of product-worded persistence ports', () => {
  const pattern = productPersistenceExportPattern()
  for (const crate of inventory.scope.ownerCrates) {
    for (const name of crateExportNames(crate)) {
      assert.doesNotMatch(name, pattern, `${crate} exports the product persistence port ${name}`)
      if (name.includes('Persistence')) {
        assert.ok(
          inventory.ports.some(port => port.trait === name),
          `${crate} exports persistence port ${name} outside the frozen inventory`,
        )
      }
    }
  }
})

test('removed Enterprise persistence types stay out of Community source and exports', () => {
  for (const trait of inventory.removedEnterprisePersistenceTraits) {
    for (const file of ownerSourceFiles()) {
      assert.doesNotMatch(
        source(file),
        new RegExp(`\\b${trait}\\b`, 'u'),
        `${file} still names the removed Enterprise persistence type ${trait}`,
      )
    }
    for (const crate of inventory.scope.ownerCrates) {
      assert.ok(!crateExportNames(crate).includes(trait), `${crate} still exports the removed type ${trait}`)
    }
  }
})

test('every frozen port carries existing Rust behavior evidence', () => {
  assert.ok(Array.isArray(inventory.evidence.ports), 'inventory evidence must list per-port cases')
  assert.deepEqual(
    inventory.evidence.ports.map(entry => entry.trait).sort(),
    inventory.ports.map(port => port.trait).sort(),
    'evidence must cover exactly the frozen ports',
  )
  for (const entry of inventory.evidence.ports) {
    assert.ok(entry.cases.length >= 1, `port ${entry.trait} carries no Rust behavior evidence`)
    for (const evidence of entry.cases) {
      assert.ok(existsSync(join(root, evidence.path)), `evidence file is missing: ${evidence.path}`)
      const text = source(evidence.path)
      assert.match(text, new RegExp(`\\b${evidence.symbol}\\b`, 'u'))
      assert.match(text, new RegExp(`\\bfn ${evidence.test}\\s*\\(`, 'u'))
    }
  }
})

test('storage dependency boundary stays product-database neutral', () => {
  const manifest = source(inventory.dependencyBoundary.manifest)
  assert.doesNotMatch(manifest, /\bgit\s*=/)
  assert.doesNotMatch(manifest, /winwincode-postgres|sqlx|tokio-postgres|deadpool-postgres|bb8-postgres/i)
  for (const match of manifest.matchAll(/\bpath\s*=\s*"([^"]+)"/g)) {
    const path = resolve(root, 'crates/winwincode-storage', match[1])
    assert.ok(path === root || path.startsWith(`${root}/`), `path dependency leaves this repository: ${match[1]}`)
  }
})
