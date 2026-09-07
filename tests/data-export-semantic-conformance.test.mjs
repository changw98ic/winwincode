import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

import Ajv2020 from 'ajv/dist/2020.js'

import {
  canonicalJsonString,
  canonicalWinWinCodeExportBytes,
  canonicalWinWinCodeExportDigestMaterialBytes,
} from '../schema/winwincode-export/v1/canonical-json.js'
import { MAX_WINWINCODE_EXPORT_BYTES } from '../schema/winwincode-export/v1/parse-bounded.js'
import {
  MAX_WINWINCODE_EXPORT_ORGANIZATIONS,
  MAX_WINWINCODE_EXPORT_PROJECTS,
  WINWINCODE_EXPORT_REJECTION_CATEGORIES,
  validateWinWinCodeExportBytes,
  validateWinWinCodeExportDocument,
} from '../schema/winwincode-export/v1/validate.js'
import { generatedWinWinCodeExportConformanceVectors } from './fixtures/data-export-conformance-vectors.mjs'

const root = resolve(import.meta.dirname, '..')
const vectorsPath = join(root, 'schema', 'winwincode-export', 'v1', 'conformance-vectors.json')
const schemaPath = join(
  root,
  'schema',
  'winwincode-export',
  'v1',
  'winwincode-export.schema.json',
)
const canonicalStringFixturePath = join(
  root,
  'schema',
  'winwincode-export',
  'v1',
  'canonical-json-string.example.json.bytes',
)
function publishedVectors() {
  return JSON.parse(readFileSync(vectorsPath, 'utf8'))
}

function organization(organizationId, slug, displayName) {
  return { sourceOrganizationId: organizationId, slug, displayName }
}

function project(projectId, organizationId, slug, displayName) {
  return { sourceProjectId: projectId, sourceOrganizationId: organizationId, slug, displayName }
}

function sealedDocument(content) {
  const document = {
    format: 'winwincode-export/v1',
    exportId: 'export_gate_01',
    contentSha256: '0'.repeat(64),
    content,
  }
  document.contentSha256 = createHash('sha256')
    .update(canonicalWinWinCodeExportDigestMaterialBytes(document))
    .digest('hex')
  return document
}

test('the Node canonical string encoder matches the shared Rust fixture byte-for-byte', () => {
  const fixture = readFileSync(canonicalStringFixturePath)
  const value = JSON.parse(fixture)
  assert.equal(value.includes('/'), true)
  assert.equal(value.includes('\\'), true)
  assert.equal(value.includes('"'), true)
  assert.equal(value.includes('é'), true)
  assert.equal(value.includes('🦀'), true)
  assert.equal(value.includes('\b'), true)
  assert.equal(value.includes('\t'), true)
  assert.equal(value.includes('\n'), true)
  assert.equal(value.includes('\f'), true)
  assert.equal(value.includes('\r'), true)
  assert.equal(value.includes('\u001f'), true)
  assert.deepEqual(Buffer.from(canonicalJsonString(value)), fixture)
})

test('the published conformance vectors reproduce from the canonical encoder', () => {
  const published = publishedVectors()
  assert.deepEqual(published, generatedWinWinCodeExportConformanceVectors())
  const ids = published.vectors.map(vector => vector.id)
  assert.equal(new Set(ids).size, ids.length, 'vector ids must be unique')
  assert.ok(ids.includes('published-fixture-is-canonical'), 'the published fixture is one vector')
  const accepts = published.vectors.filter(vector => vector.expectation === 'accept')
  const rejects = published.vectors.filter(vector => vector.expectation === 'reject')
  assert.ok(accepts.length >= 5, `expected several accepting vectors, found ${accepts.length}`)
  assert.ok(rejects.length >= 30, `expected many rejecting vectors, found ${rejects.length}`)
})

test('the non-Rust strict gate agrees with every published conformance vector', () => {
  const { vectors } = publishedVectors()
  const rejectedCategories = new Set()
  for (const vector of vectors) {
    const bytes = Buffer.from(vector.bytesHex, 'hex')
    const result = validateWinWinCodeExportBytes(bytes)
    if (vector.expectation === 'accept') {
      assert.equal(vector.category, null, vector.id)
      assert.equal(result.status, 'accepted', `${vector.id}: ${JSON.stringify(result)}`)
      assert.deepEqual(result.document, JSON.parse(bytes), vector.id)
      assert.equal(result.contentSha256, result.document.contentSha256, vector.id)
    } else {
      assert.equal(result.status, 'rejected', `${vector.id}: ${JSON.stringify(result)}`)
      assert.equal(result.category, vector.category, vector.id)
      assert.equal(WINWINCODE_EXPORT_REJECTION_CATEGORIES.includes(result.category), true, vector.id)
      assert.equal(typeof result.message, 'string', vector.id)
      rejectedCategories.add(result.category)
    }
  }
  assert.deepEqual(
    [...rejectedCategories].sort(),
    [...new Set(vectors.filter(vector => vector.category).map(vector => vector.category))].sort(),
  )
})

test('the non-Rust strict gate enforces the record count limits', () => {
  const tooManyOrganizations = validateWinWinCodeExportDocument(sealedDocument({
    profileDisplayName: 'Profile',
    organizations: Array.from(
      { length: MAX_WINWINCODE_EXPORT_ORGANIZATIONS + 1 },
      (_, index) => organization(`org_${index.toString().padStart(10, '0')}`, `org-${index}`, 'Organization'),
    ),
    projects: [],
  }))
  assert.equal(tooManyOrganizations.status, 'rejected')
  assert.equal(tooManyOrganizations.category, 'invalid-content')

  const tooManyProjects = validateWinWinCodeExportDocument(sealedDocument({
    profileDisplayName: 'Profile',
    organizations: [organization(
      'org_00000000000000000000000001',
      'org-00000000000000000000000001',
      'Organization',
    )],
    projects: Array.from(
      { length: MAX_WINWINCODE_EXPORT_PROJECTS + 1 },
      (_, index) => project(
        `prj_${index.toString().padStart(10, '0')}`,
        'org_00000000000000000000000001',
        `project-${index}`,
        'Project',
      ),
    ),
  }))
  assert.equal(tooManyProjects.status, 'rejected')
  assert.equal(tooManyProjects.category, 'invalid-content')
})

test('the strict gate accepts the byte limit and rejects the next byte', () => {
  const organizationId = 'org_00000000000000000000000001'
  const content = {
    profileDisplayName: 'Profile',
    organizations: [organization(organizationId, 'org-00000000000000000000000001', 'Organization')],
    projects: Array.from({ length: 70_000 }, (_, index) => project(
      `prj_${index.toString(36).padStart(26, '0')}`,
      organizationId,
      `project-${index}`,
      'P',
    )),
  }
  const base = Buffer.from(canonicalWinWinCodeExportBytes({
    format: 'winwincode-export/v1',
    exportId: 'export_gate_01',
    contentSha256: '0'.repeat(64),
    content,
  }))
  let remaining = MAX_WINWINCODE_EXPORT_BYTES - base.byteLength
  assert.ok(remaining > 0, 'the document base must be smaller than the byte limit')
  for (const record of content.projects) {
    const added = Math.min(remaining, 255)
    record.displayName += 'x'.repeat(added)
    remaining -= added
    if (remaining === 0) break
  }
  assert.equal(remaining, 0, 'the document lacks capacity to reach the byte limit')
  const atLimit = Buffer.from(canonicalWinWinCodeExportBytes(sealedDocument(content)))
  assert.equal(atLimit.byteLength, MAX_WINWINCODE_EXPORT_BYTES)

  const accepted = validateWinWinCodeExportBytes(atLimit)
  assert.equal(accepted.status, 'accepted', JSON.stringify(accepted))

  const rejected = validateWinWinCodeExportBytes(Buffer.concat([atLimit, Buffer.from('x')]))
  assert.equal(rejected.status, 'rejected')
  assert.equal(rejected.category, 'too-large')
})

test('the published schema alone still accepts what the strict gate rejects', () => {
  const validate = new Ajv2020({ allErrors: true, strict: true })
    .compile(JSON.parse(readFileSync(schemaPath, 'utf8')))
  const vectorById = new Map(
    publishedVectors().vectors.map(vector => [vector.id, vector]),
  )
  const structuralOnly = [
    'duplicate-organization-source-id',
    'duplicate-project-source-id',
    'dangling-project-organization-reference',
    'organizations-out-of-canonical-order',
    'digest-of-all-zeros',
    'pretty-printed-document',
  ]
  for (const id of structuralOnly) {
    const bytes = Buffer.from(vectorById.get(id).bytesHex, 'hex')
    assert.equal(validate(JSON.parse(bytes)), true, `${id} is structurally valid`)
    const result = validateWinWinCodeExportBytes(bytes)
    assert.equal(result.status, 'rejected', `${id} must fail the strict gate`)
  }
})
