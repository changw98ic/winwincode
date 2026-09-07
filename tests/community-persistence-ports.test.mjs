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

function traitBlock(text, name) {
  const start = text.search(new RegExp(`^pub trait\\s+${name}\\b`, 'm'))
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
      const marked = new Set([...text.matchAll(/^#\[doc = "winwincode-community-persistence-port"\]\s*\npub trait\s+(\w+)/gm)].map(match => match[1]))
      for (const match of text.matchAll(/^pub trait\s+(\w+)/gm)) {
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

test('Community inventory matches the public persistence ports', () => {
  assert.equal(inventory.schemaVersion, 1)
  assert.equal(inventory.task, 'winwincode-edition.2.4.1.2')
  assert.equal(inventory.scope.portCount, inventory.ports.length)
  assert.deepEqual(discoveredPorts(), inventory.ports.map(({ crate, trait, source }) => ({ crate, trait, source })))
  for (const entry of inventory.ports) {
    assert.ok(existsSync(join(root, entry.source)), `port source is missing: ${entry.source}`)
    assert.deepEqual(resultErrors(traitBlock(source(entry.source), entry.trait)), entry.stableErrors)
  }
})

test('public persistence ports reject product-specific storage types', () => {
  const forbidden = inventory.contract.forbiddenPublicTypePatterns
  for (const entry of inventory.ports) {
    const block = traitBlock(source(entry.source), entry.trait)
    for (const pattern of forbidden) assert.doesNotMatch(block, new RegExp(`\\b${pattern}\\b`, 'i'), `${entry.trait} exposes ${pattern}`)
  }
  for (const trait of inventory.removedEnterprisePersistenceTraits) {
    assert.doesNotMatch(discoveredPorts().map(entry => entry.trait).join('\n'), new RegExp(`^${trait}$`, 'm'))
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

test('inventory SQLite evidence remains present', () => {
  for (const evidence of inventory.evidence.sqlite) {
    const text = source(evidence.path)
    assert.match(text, new RegExp(`\\b${evidence.symbol}\\b`))
    assert.match(text, new RegExp(`\\bfn ${evidence.test}\\s*\\(`))
  }
})
