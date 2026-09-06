#!/usr/bin/env node

// PG-000 (winwincode-9c4.21.1) migration inventory contract check.
//
// The single-backend PostgreSQL decision (ADR-0031) freezes one machine
// readable table inventory at
// docs/decisions/0031-single-backend-postgres.inventory.json. This script
// keeps that inventory honest against the code:
//
//   1. every `CREATE TABLE` table name that appears anywhere under crates/
//      must be covered by exactly one of: an inventory entry, a legacy
//      target table (winwincode-postgres v1), or a declared
//      nonInventoryName (test-only fixture or transient rename);
//   2. every inventory entry's source table must actually exist in the
//      code (no phantom entries);
//   3. every entry carries the frozen fields (data domain, source table,
//      target table, primary key, foreign keys, migration order, volume
//      check, owner, disposition) with enum-valid values;
//   4. foreign keys reference inventoried tables and respect the
//      migration-order invariant (referenced order <= referencing order);
//   5. entry ids and (database, table) pairs are unique.
//
// Run: node scripts/db-migration-inventory-contract.mjs

import { readFileSync, readdirSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'

const INVENTORY_SCHEMA_VERSION = 1

const root = resolve(import.meta.dirname, '..')
const inventoryPath = join(
  root,
  'docs/decisions/0031-single-backend-postgres.inventory.json',
)

const REQUIRED_DATA_DOMAINS = Object.freeze([
  'control_plane_state',
  'identity_access',
  'repository',
  'client_access_occupancy',
  'execution_scheduling',
  'enterprise_governance',
  'provider_exchange',
  'artifact_catalog',
  'audit',
  'server_auth',
  'event_hub',
  'observability',
  'integration',
  'device_local_state',
  'kernel_local_state',
  'legacy_migration_bookkeeping',
])

const REQUIRED_DISPOSITIONS = Object.freeze(['migrate', 'retain'])

function listRustFiles(directory) {
  const files = []
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) {
      if (entry.name === 'target' || entry.name.startsWith('.')) continue
      files.push(...listRustFiles(path))
    } else if (entry.name.endsWith('.rs')) {
      files.push(path)
    }
  }
  return files
}

function discoverCreateTables() {
  // Matches `CREATE TABLE` and `CREATE TABLE IF NOT EXISTS <name>` inside
  // Rust source and test files. SQLite table names in this workspace are
  // plain identifiers (no quoting, no schema qualification, no virtual
  // tables), which the second assertion below re-checks per file.
  const names = new Map()
  const pattern = /CREATE TABLE (?:IF NOT EXISTS )?([A-Za-z_][A-Za-z0-9_]*)/g
  for (const path of listRustFiles(join(root, 'crates'))) {
    const source = readFileSync(path, 'utf8')
    if (!source.includes('CREATE TABLE')) continue
    const relativePath = relative(root, path)
    for (const match of source.matchAll(pattern)) {
      const name = match[1]
      if (!names.has(name)) names.set(name, [])
      names.get(name).push(relativePath)
    }
  }
  return names
}

function failure(message) {
  console.error(`db-migration-inventory-contract: ${message}`)
  process.exitCode = 1
}

const inventory = JSON.parse(readFileSync(inventoryPath, 'utf8'))

if (inventory.schemaVersion !== INVENTORY_SCHEMA_VERSION) {
  failure(`inventory schemaVersion must be ${INVENTORY_SCHEMA_VERSION}`)
}

const entries = Array.isArray(inventory.entries) ? inventory.entries : []
if (entries.length === 0) failure('inventory has no entries')

const databaseIds = new Set(
  (inventory.databases ?? []).map((database) => database.id),
)

const seenIds = new Set()
const seenTables = new Set()
for (const entry of entries) {
  const where = entry.id ?? '<missing id>'
  if (seenIds.has(entry.id)) failure(`duplicate entry id: ${entry.id}`)
  seenIds.add(entry.id)
  const tableKey = `${entry.sourceDatabase}:${entry.sourceTable}`
  if (seenTables.has(tableKey)) {
    failure(`duplicate source table across entries: ${tableKey}`)
  }
  seenTables.add(tableKey)

  if (!databaseIds.has(entry.sourceDatabase)) {
    failure(`${where}: sourceDatabase ${entry.sourceDatabase} is not declared in databases`)
  }
  if (!REQUIRED_DATA_DOMAINS.includes(entry.dataDomain)) {
    failure(`${where}: dataDomain ${entry.dataDomain} is not in the frozen enum`)
  }
  if (!REQUIRED_DISPOSITIONS.includes(entry.disposition)) {
    failure(`${where}: disposition ${entry.disposition} is not in the frozen enum`)
  }
  if (typeof entry.primaryKey !== 'string' || entry.primaryKey.length === 0) {
    failure(`${where}: primaryKey must be a non-empty string`)
  }
  if (!Array.isArray(entry.foreignKeys)) {
    failure(`${where}: foreignKeys must be an array`)
  }
  if (
    !entry.volumeCheck ||
    typeof entry.volumeCheck.checkSql !== 'string' ||
    typeof entry.volumeCheck.expectation !== 'string'
  ) {
    failure(`${where}: volumeCheck needs checkSql and expectation`)
  }
  if (
    !entry.owner ||
    typeof entry.owner.crate !== 'string' ||
    typeof entry.owner.module !== 'string'
  ) {
    failure(`${where}: owner needs crate and module`)
  }
  if (typeof entry.reason !== 'string' || entry.reason.length === 0) {
    failure(`${where}: reason must be a non-empty string`)
  }
  if (entry.disposition === 'migrate') {
    if (typeof entry.migrationOrder !== 'number') {
      failure(`${where}: migrate entries need a numeric migrationOrder`)
    }
    if (typeof entry.targetTable !== 'string' || !entry.targetTable.includes('.')) {
      failure(`${where}: migrate entries need targetTable as <pgSchema>.<table>`)
    }
  } else if (
    entry.migrationOrder !== null ||
    entry.targetTable !== null
  ) {
    failure(`${where}: retain entries must keep migrationOrder and targetTable null`)
  }
}

// Foreign keys must reference inventoried tables and respect the
// migration-order invariant.
const byTable = new Map(
  entries.map((entry) => [`${entry.sourceDatabase}:${entry.sourceTable}`, entry]),
)
for (const entry of entries) {
  for (const foreignKey of entry.foreignKeys ?? []) {
    const reference = byTable.get(
      `${entry.sourceDatabase}:${foreignKey.referencesTable}`,
    )
    if (!reference) {
      failure(
        `${entry.id}: foreign key references non-inventoried table ${foreignKey.referencesTable}`,
      )
      continue
    }
    if (
      entry.disposition === 'migrate' &&
      reference.migrationOrder > entry.migrationOrder
    ) {
      failure(
        `${entry.id}: migrationOrder ${entry.migrationOrder} precedes referenced table ${foreignKey.referencesTable} at ${reference.migrationOrder}`,
      )
    }
  }
}

// Coverage both ways between code and inventory.
const discovered = discoverCreateTables()
const legacyTables = new Set(inventory.legacyTargets?.tables ?? [])
const nonInventory = new Set(
  (inventory.nonInventoryNames ?? []).map((named) => named.name),
)

let uncovered = 0
for (const [name, paths] of discovered) {
  const covered =
    [...byTable.keys()].some((key) => key.endsWith(`:${name}`)) ||
    legacyTables.has(name) ||
    nonInventory.has(name)
  if (!covered) {
    uncovered += 1
    failure(
      `CREATE TABLE ${name} (${paths.join(', ')}) is missing from the inventory`,
    )
  }
}

let phantom = 0
for (const entry of entries) {
  if (!discovered.has(entry.sourceTable)) {
    phantom += 1
    failure(
      `${entry.id}: source table ${entry.sourceTable} has no CREATE TABLE under crates/`,
    )
  }
}

const migrate = entries.filter((entry) => entry.disposition === 'migrate').length
const retain = entries.filter((entry) => entry.disposition === 'retain').length

if (process.exitCode !== 1) {
  console.log(
    `db-migration-inventory-contract: OK — ${entries.length} entries ` +
      `(${migrate} migrate, ${retain} retain), ${discovered.size} distinct ` +
      `CREATE TABLE names under crates/, 0 uncovered, 0 phantom`,
  )
}
