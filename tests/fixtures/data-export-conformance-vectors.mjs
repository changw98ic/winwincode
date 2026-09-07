import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

import {
  canonicalWinWinCodeExportBytes,
  canonicalWinWinCodeExportDigestMaterialBytes,
} from '../../schema/winwincode-export/v1/canonical-json.js'
import { WINWINCODE_EXPORT_FORMAT } from '../../schema/winwincode-export/v1/validate.js'

const root = resolve(import.meta.dirname, '../..')
const fixturePath = join(
  root,
  'schema',
  'winwincode-export',
  'v1',
  'winwincode-export.example.json.bytes',
)

const utf8Encoder = new TextEncoder()
const utf8Decoder = new TextDecoder()

function organization(organizationId, slug, displayName) {
  return { sourceOrganizationId: organizationId, slug, displayName }
}

function project(projectId, organizationId, slug, displayName) {
  return { sourceProjectId: projectId, sourceOrganizationId: organizationId, slug, displayName }
}

function unsealedDocument(content, overrides = {}) {
  return {
    format: WINWINCODE_EXPORT_FORMAT,
    exportId: 'export_gate_01',
    contentSha256: '0'.repeat(64),
    content,
    ...overrides,
  }
}

function sealedDocument(content, overrides = {}) {
  const document = unsealedDocument(content, overrides)
  document.contentSha256 = createHash('sha256')
    .update(canonicalWinWinCodeExportDigestMaterialBytes(document))
    .digest('hex')
  return document
}

function canonicalBytes(document) {
  return Buffer.from(canonicalWinWinCodeExportBytes(document))
}

function replacedBytes(bytes, from, to) {
  const text = utf8Decoder.decode(bytes)
  assert.ok(text.includes(from), `document text must contain ${JSON.stringify(from)}`)
  return Buffer.from(utf8Encoder.encode(text.replace(from, to)))
}

function splicedBytes(bytes, from, to) {
  const start = bytes.indexOf(from)
  assert.notEqual(start, -1, 'document bytes must contain the spliced source')
  return Buffer.concat([
    bytes.subarray(0, start),
    Buffer.from(to),
    bytes.subarray(start + from.length),
  ])
}

/** Exchanges two canonical record texts, because the encoder itself always sorts. */
function swappedRecords(bytes, left, right) {
  const text = utf8Decoder.decode(bytes)
  const leftText = JSON.stringify(left)
  const rightText = JSON.stringify(right)
  assert.ok(text.includes(leftText), 'document bytes must contain the left record')
  assert.ok(text.includes(rightText), 'document bytes must contain the right record')
  const swapped = text
    .replace(leftText, '\u0000left\u0000')
    .replace(rightText, leftText)
    .replace('\u0000left\u0000', rightText)
  return Buffer.from(utf8Encoder.encode(swapped))
}

function withDigest(bytes, digest) {
  return replacedBytes(
    bytes,
    utf8Decoder.decode(bytes).match(/"contentSha256":"[0-9a-f]{64}"/u)[0],
    `"contentSha256":"${digest}"`,
  )
}

const ORGANIZATION_ONE = organization(
  'org_00000000000000000000000001',
  'org-00000000000000000000000001',
  '组织 / café 🦀',
)
const ORGANIZATION_TWO = organization(
  'org_00000000000000000000000002',
  'org-00000000000000000000000002',
  'Second Organization',
)

function singleOrganizationContent(overrides = {}) {
  return {
    profileDisplayName: 'Profile',
    organizations: [ORGANIZATION_ONE],
    projects: [],
    ...overrides,
  }
}

const VECTOR_BUILDERS = Object.freeze([
  {
    id: 'published-fixture-is-canonical',
    expectation: 'accept',
    build: () => readFileSync(fixturePath),
  },
  {
    id: 'empty-content-is-canonical',
    expectation: 'accept',
    build: () => canonicalBytes(sealedDocument({
      profileDisplayName: 'Profile',
      organizations: [],
      projects: [],
    })),
  },
  {
    id: 'multiple-records-in-canonical-order',
    expectation: 'accept',
    build: () => canonicalBytes(sealedDocument({
      profileDisplayName: 'Profile',
      organizations: [ORGANIZATION_ONE, ORGANIZATION_TWO],
      projects: [
        project('prj_00000000000000000000000001', ORGANIZATION_ONE.sourceOrganizationId, 'first-project', 'First'),
        project('prj_00000000000000000000000002', ORGANIZATION_ONE.sourceOrganizationId, 'second-project', 'Second'),
        project('prj_00000000000000000000000003', ORGANIZATION_TWO.sourceOrganizationId, 'third-project', 'Third'),
      ],
    })),
  },
  {
    id: 'project-slugs-are-unique-per-organization',
    expectation: 'accept',
    build: () => canonicalBytes(sealedDocument({
      profileDisplayName: 'Profile',
      organizations: [ORGANIZATION_ONE, ORGANIZATION_TWO],
      projects: [
        project('prj_00000000000000000000000001', ORGANIZATION_ONE.sourceOrganizationId, 'shared-slug', 'First'),
        project('prj_00000000000000000000000002', ORGANIZATION_TWO.sourceOrganizationId, 'shared-slug', 'Second'),
      ],
    })),
  },
  {
    id: 'display-name-at-256-code-points-is-canonical',
    expectation: 'accept',
    build: () => canonicalBytes(sealedDocument(singleOrganizationContent({
      profileDisplayName: '界'.repeat(256),
    }))),
  },
  {
    id: 'record-order-compares-unicode-scalar-values',
    expectation: 'accept',
    build: () => canonicalBytes(sealedDocument({
      profileDisplayName: 'Profile',
      organizations: [
        organization('org_', 'private-use', 'Private Use'),
        organization('org_🦀', 'crab', 'Crab'),
      ],
      projects: [],
    })),
  },
  {
    id: 'utf16-code-unit-ordering-is-not-canonical',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => swappedRecords(
      canonicalBytes(sealedDocument({
        profileDisplayName: 'Profile',
        organizations: [
          organization('org_', 'private-use', 'Private Use'),
          organization('org_🦀', 'crab', 'Crab'),
        ],
        projects: [],
      })),
      organization('org_', 'private-use', 'Private Use'),
      organization('org_🦀', 'crab', 'Crab'),
    ),
  },
  {
    id: 'duplicate-organization-source-id',
    expectation: 'reject',
    category: 'duplicate-source-identifier',
    build: () => canonicalBytes(sealedDocument({
      profileDisplayName: 'Profile',
      organizations: [ORGANIZATION_ONE, { ...ORGANIZATION_ONE, slug: 'second-slug' }],
      projects: [],
    })),
  },
  {
    id: 'duplicate-organization-slug',
    expectation: 'reject',
    category: 'duplicate-source-identifier',
    build: () => canonicalBytes(sealedDocument({
      profileDisplayName: 'Profile',
      organizations: [ORGANIZATION_ONE, { ...ORGANIZATION_TWO, slug: ORGANIZATION_ONE.slug }],
      projects: [],
    })),
  },
  {
    id: 'duplicate-project-source-id',
    expectation: 'reject',
    category: 'duplicate-source-identifier',
    build: () => canonicalBytes(sealedDocument({
      profileDisplayName: 'Profile',
      organizations: [ORGANIZATION_ONE, ORGANIZATION_TWO],
      projects: [
        project('prj_00000000000000000000000009', ORGANIZATION_ONE.sourceOrganizationId, 'first-project', 'First'),
        project('prj_00000000000000000000000009', ORGANIZATION_TWO.sourceOrganizationId, 'second-project', 'Second'),
      ],
    })),
  },
  {
    id: 'duplicate-project-slug-in-one-organization',
    expectation: 'reject',
    category: 'duplicate-source-identifier',
    build: () => canonicalBytes(sealedDocument({
      profileDisplayName: 'Profile',
      organizations: [ORGANIZATION_ONE],
      projects: [
        project('prj_00000000000000000000000001', ORGANIZATION_ONE.sourceOrganizationId, 'shared-slug', 'First'),
        project('prj_00000000000000000000000002', ORGANIZATION_ONE.sourceOrganizationId, 'shared-slug', 'Second'),
      ],
    })),
  },
  {
    id: 'dangling-project-organization-reference',
    expectation: 'reject',
    category: 'unknown-source-organization',
    build: () => canonicalBytes(sealedDocument(singleOrganizationContent({
      projects: [
        project(
          'prj_00000000000000000000000001',
          'org_99999999999999999999999999',
          'orphan-project',
          'Orphan',
        ),
      ],
    }))),
  },
  {
    id: 'organizations-out-of-canonical-order',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => swappedRecords(
      canonicalBytes(sealedDocument({
        profileDisplayName: 'Profile',
        organizations: [ORGANIZATION_ONE, ORGANIZATION_TWO],
        projects: [],
      })),
      ORGANIZATION_ONE,
      ORGANIZATION_TWO,
    ),
  },
  {
    id: 'projects-out-of-canonical-order',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => {
      const first = project(
        'prj_00000000000000000000000001',
        ORGANIZATION_ONE.sourceOrganizationId,
        'first-project',
        'First',
      )
      const second = project(
        'prj_00000000000000000000000002',
        ORGANIZATION_ONE.sourceOrganizationId,
        'second-project',
        'Second',
      )
      return swappedRecords(
        canonicalBytes(sealedDocument(singleOrganizationContent({ projects: [first, second] }))),
        first,
        second,
      )
    },
  },
  {
    id: 'project-groups-out-of-organization-order',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => {
      const firstOrganizationProject = project(
        'prj_00000000000000000000000002',
        ORGANIZATION_ONE.sourceOrganizationId,
        'second-project',
        'Second',
      )
      const secondOrganizationProject = project(
        'prj_00000000000000000000000003',
        ORGANIZATION_TWO.sourceOrganizationId,
        'third-project',
        'Third',
      )
      return swappedRecords(
        canonicalBytes(sealedDocument({
          profileDisplayName: 'Profile',
          organizations: [ORGANIZATION_ONE, ORGANIZATION_TWO],
          projects: [firstOrganizationProject, secondOrganizationProject],
        })),
        firstOrganizationProject,
        secondOrganizationProject,
      )
    },
  },
  {
    id: 'digest-of-all-zeros',
    expectation: 'reject',
    category: 'digest-mismatch',
    build: () => canonicalBytes(unsealedDocument(singleOrganizationContent())),
  },
  {
    id: 'altered-record-without-digest-update',
    expectation: 'reject',
    category: 'digest-mismatch',
    build: () => canonicalBytes(unsealedDocument(singleOrganizationContent({
      organizations: [{ ...ORGANIZATION_ONE, displayName: 'Renamed Organization' }],
    }))),
  },
  {
    id: 'digest-of-another-document',
    expectation: 'reject',
    category: 'digest-mismatch',
    build: () => {
      const sealed = sealedDocument(singleOrganizationContent())
      const other = utf8Decoder.decode(canonicalBytes(sealedDocument({
        profileDisplayName: 'Other Profile',
        organizations: [ORGANIZATION_ONE],
        projects: [],
      })))
      const digest = other.match(/"contentSha256":"([0-9a-f]{64})"/u)[1]
      return canonicalBytes({ ...sealed, contentSha256: digest })
    },
  },
  {
    id: 'digest-with-uppercase-hex',
    expectation: 'reject',
    category: 'digest-mismatch',
    build: () => {
      const sealed = sealedDocument(singleOrganizationContent())
      return canonicalBytes({ ...sealed, contentSha256: sealed.contentSha256.toUpperCase() })
    },
  },
  {
    id: 'pretty-printed-document',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => Buffer.from(JSON.stringify(sealedDocument(singleOrganizationContent()), null, 2)),
  },
  {
    id: 'organization-member-order-is-not-canonical',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => {
      const text = utf8Decoder.decode(canonicalBytes(sealedDocument(singleOrganizationContent())))
      const quoted = value => JSON.stringify(value)
      const canonicalOrganization = `{"sourceOrganizationId":${quoted(ORGANIZATION_ONE.sourceOrganizationId)},`
        + `"slug":${quoted(ORGANIZATION_ONE.slug)},`
        + `"displayName":${quoted(ORGANIZATION_ONE.displayName)}}`
      const reorderedOrganization = `{"slug":${quoted(ORGANIZATION_ONE.slug)},`
        + `"sourceOrganizationId":${quoted(ORGANIZATION_ONE.sourceOrganizationId)},`
        + `"displayName":${quoted(ORGANIZATION_ONE.displayName)}}`
      assert.ok(text.includes(canonicalOrganization), 'document holds the canonical organization')
      return Buffer.from(utf8Encoder.encode(text.replace(canonicalOrganization, reorderedOrganization)))
    },
  },
  {
    id: 'escaped-solidus-is-not-canonical',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => replacedBytes(
      canonicalBytes(sealedDocument(singleOrganizationContent())),
      ' / ',
      ' \\/ ',
    ),
  },
  {
    id: 'non-canonical-unicode-escape',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => replacedBytes(
      canonicalBytes(sealedDocument({
        profileDisplayName: 'café Profile',
        organizations: [ORGANIZATION_ONE],
        projects: [],
      })),
      'é',
      '\\u00e9',
    ),
  },
  {
    id: 'astral-escape-is-not-canonical',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => replacedBytes(
      canonicalBytes(sealedDocument({
        profileDisplayName: '🦀 Profile',
        organizations: [ORGANIZATION_ONE],
        projects: [],
      })),
      '🦀',
      '\\ud83e\\udd80',
    ),
  },
  {
    id: 'trailing-newline-is-not-canonical',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => Buffer.concat([
      canonicalBytes(sealedDocument(singleOrganizationContent())),
      Buffer.from('\n'),
    ]),
  },
  {
    id: 'leading-whitespace-is-not-canonical',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => Buffer.concat([
      Buffer.from(' '),
      canonicalBytes(sealedDocument(singleOrganizationContent())),
    ]),
  },
  {
    id: 'unsupported-format',
    expectation: 'reject',
    category: 'unsupported-format',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent(),
      { format: 'winwincode-export/v2' },
    )),
  },
  {
    id: 'export-id-is-empty',
    expectation: 'reject',
    category: 'invalid-export-id',
    build: () => canonicalBytes(sealedDocument(singleOrganizationContent(), { exportId: '' })),
  },
  {
    id: 'export-id-has-forbidden-character',
    expectation: 'reject',
    category: 'invalid-export-id',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent(),
      { exportId: 'export/gate' },
    )),
  },
  {
    id: 'export-id-exceeds-128-characters',
    expectation: 'reject',
    category: 'invalid-export-id',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent(),
      { exportId: 'a'.repeat(129) },
    )),
  },
  {
    id: 'display-name-is-empty',
    expectation: 'reject',
    category: 'invalid-content',
    build: () => canonicalBytes(sealedDocument(singleOrganizationContent({ profileDisplayName: '' }))),
  },
  {
    id: 'display-name-has-leading-whitespace',
    expectation: 'reject',
    category: 'invalid-content',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent({ profileDisplayName: ' Profile' }),
    )),
  },
  {
    id: 'display-name-has-leading-byte-order-mark',
    expectation: 'reject',
    category: 'invalid-content',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent({ profileDisplayName: ' Profile' }),
    )),
  },
  {
    id: 'display-name-has-control-character',
    expectation: 'reject',
    category: 'invalid-content',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent({ profileDisplayName: 'Profile\u0007Bell' }),
    )),
  },
  {
    id: 'display-name-exceeds-256-code-points',
    expectation: 'reject',
    category: 'invalid-content',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent({ profileDisplayName: '界'.repeat(257) }),
    )),
  },
  {
    id: 'slug-has-uppercase',
    expectation: 'reject',
    category: 'invalid-content',
    build: () => canonicalBytes(sealedDocument(singleOrganizationContent({
      organizations: [organization(ORGANIZATION_ONE.sourceOrganizationId, 'Org-Slug', 'Organization')],
    }))),
  },
  {
    id: 'slug-has-leading-hyphen',
    expectation: 'reject',
    category: 'invalid-content',
    build: () => canonicalBytes(sealedDocument(singleOrganizationContent({
      organizations: [organization(ORGANIZATION_ONE.sourceOrganizationId, '-org', 'Organization')],
    }))),
  },
  {
    id: 'slug-exceeds-128-characters',
    expectation: 'reject',
    category: 'invalid-content',
    build: () => canonicalBytes(sealedDocument(singleOrganizationContent({
      organizations: [
        organization(ORGANIZATION_ONE.sourceOrganizationId, 'a'.repeat(129), 'Organization'),
      ],
    }))),
  },
  {
    id: 'source-id-exceeds-256-characters',
    expectation: 'reject',
    category: 'invalid-content',
    build: () => canonicalBytes(sealedDocument(singleOrganizationContent({
      organizations: [organization('a'.repeat(257), 'long-source', 'Organization')],
    }))),
  },
  {
    id: 'display-name-is-unix-absolute-path',
    expectation: 'reject',
    category: 'local-path-not-allowed',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent({ profileDisplayName: '/Users/example/private' }),
    )),
  },
  {
    id: 'display-name-is-windows-absolute-path',
    expectation: 'reject',
    category: 'local-path-not-allowed',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent({ profileDisplayName: 'C:\\private\\repository' }),
    )),
  },
  {
    id: 'display-name-is-home-relative-path',
    expectation: 'reject',
    category: 'local-path-not-allowed',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent({ profileDisplayName: '~/private/repository' }),
    )),
  },
  {
    id: 'display-name-is-file-url',
    expectation: 'reject',
    category: 'local-path-not-allowed',
    build: () => canonicalBytes(sealedDocument(
      singleOrganizationContent({ profileDisplayName: 'file:///etc/passwd' }),
    )),
  },
  {
    id: 'document-has-unknown-member',
    expectation: 'reject',
    category: 'invalid-json',
    build: () => replacedBytes(
      canonicalBytes(sealedDocument(singleOrganizationContent())),
      '{"format":',
      '{"exportedAt":"2026-09-07","format":',
    ),
  },
  {
    id: 'document-is-missing-a-member',
    expectation: 'reject',
    category: 'invalid-json',
    build: () => {
      const text = utf8Decoder.decode(canonicalBytes(sealedDocument(singleOrganizationContent())))
      const digestMember = text.match(/,"contentSha256":"[0-9a-f]{64}"/u)[0]
      return replacedBytes(
        canonicalBytes(sealedDocument(singleOrganizationContent())),
        digestMember,
        '',
      )
    },
  },
  {
    id: 'document-is-truncated',
    expectation: 'reject',
    category: 'invalid-json',
    build: () => {
      const bytes = canonicalBytes(sealedDocument(singleOrganizationContent()))
      return bytes.subarray(0, bytes.byteLength - 4)
    },
  },
  {
    id: 'document-holds-a-lone-surrogate',
    expectation: 'reject',
    category: 'invalid-json',
    build: () => replacedBytes(
      canonicalBytes(sealedDocument(singleOrganizationContent())),
      '"Profile"',
      '"Profile\\ud800"',
    ),
  },
  {
    id: 'document-is-invalid-utf8',
    expectation: 'reject',
    category: 'invalid-json',
    build: () => splicedBytes(
      canonicalBytes(sealedDocument({
        profileDisplayName: 'café Profile',
        organizations: [ORGANIZATION_ONE],
        projects: [],
      })),
      Buffer.from('é', 'utf8'),
      Buffer.from([0xc3]),
    ),
  },
  {
    id: 'non-canonical-order-precedes-digest',
    expectation: 'reject',
    category: 'non-canonical',
    build: () => withDigest(
      swappedRecords(
        canonicalBytes(sealedDocument({
          profileDisplayName: 'Profile',
          organizations: [ORGANIZATION_ONE, ORGANIZATION_TWO],
          projects: [],
        })),
        ORGANIZATION_ONE,
        ORGANIZATION_TWO,
      ),
      '0'.repeat(64),
    ),
  },
  {
    id: 'duplicate-identifier-precedes-digest',
    expectation: 'reject',
    category: 'duplicate-source-identifier',
    build: () => canonicalBytes(unsealedDocument({
      profileDisplayName: 'Profile',
      organizations: [ORGANIZATION_ONE, { ...ORGANIZATION_ONE, slug: 'second-slug' }],
      projects: [],
    })),
  },
  {
    id: 'unknown-reference-precedes-order',
    expectation: 'reject',
    category: 'unknown-source-organization',
    build: () => swappedRecords(
      canonicalBytes(sealedDocument({
        profileDisplayName: 'Profile',
        organizations: [ORGANIZATION_ONE, ORGANIZATION_TWO],
        projects: [
          project(
            'prj_00000000000000000000000001',
            'org_99999999999999999999999999',
            'orphan-project',
            'Orphan',
          ),
        ],
      })),
      ORGANIZATION_ONE,
      ORGANIZATION_TWO,
    ),
  },
  {
    id: 'unsupported-format-precedes-content',
    expectation: 'reject',
    category: 'unsupported-format',
    build: () => canonicalBytes(unsealedDocument(
      singleOrganizationContent({ profileDisplayName: ' Profile' }),
      { format: 'winwincode-export/v2', exportId: 'export/gate' },
    )),
  },
])

/** Builds the complete published vector set from the canonical encoder. */
export function generatedWinWinCodeExportConformanceVectors() {
  return {
    schema: 'winwincode-export-conformance-vectors/v1',
    description: 'Positive and negative conformance vectors shared by the Rust contract owner and'
      + ' the non-Rust strict gate. Each vector holds one complete document; both implementations'
      + ' must return the same verdict and, for a rejection, the same category.',
    vectors: VECTOR_BUILDERS.map(({ id, expectation, category = null, build }) => ({
      id,
      expectation,
      category,
      bytesHex: build().toString('hex'),
    })),
  }
}
