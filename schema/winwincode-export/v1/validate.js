// SPDX-License-Identifier: Apache-2.0

import { createHash } from 'node:crypto'
import {
  canonicalWinWinCodeExportBytes,
  canonicalWinWinCodeExportDigestMaterialBytes,
  compareUnicodeScalars,
} from './canonical-json.js'
import {
  MAX_WINWINCODE_EXPORT_BYTES,
  parseBoundedWinWinCodeExportJson,
} from './parse-bounded.js'

/** The only format identifier accepted by `winwincode-export/v1`. */
export const WINWINCODE_EXPORT_FORMAT = 'winwincode-export/v1'

/** Maximum number of organization records in one v1 document. */
export const MAX_WINWINCODE_EXPORT_ORGANIZATIONS = 10_000

/** Maximum number of project records in one v1 document. */
export const MAX_WINWINCODE_EXPORT_PROJECTS = 100_000

/** Rejection categories of the strict v1 gate, in reporting precedence. */
export const WINWINCODE_EXPORT_REJECTION_CATEGORIES = Object.freeze([
  'too-large',
  'invalid-json',
  'unsupported-format',
  'invalid-export-id',
  'invalid-content',
  'local-path-not-allowed',
  'duplicate-source-identifier',
  'unknown-source-organization',
  'digest-mismatch',
  'non-canonical',
])

const MAXIMUM_EXPORT_ID_CHARACTERS = 128
const MAXIMUM_SOURCE_ID_CHARACTERS = 256
const MAXIMUM_DISPLAY_NAME_CHARACTERS = 256
const MAXIMUM_SLUG_CHARACTERS = 128
const LOWERCASE_HEX = '0123456789abcdef'
const EXPORT_ID_PATTERN = /^[A-Za-z0-9_.:-]+$/u

function rejection(category, message) {
  return { status: 'rejected', category, message }
}

function rejected(problem) {
  return rejection(problem.category, problem.message)
}

function contentProblem(category, message) {
  return { category, message }
}

/**
 * Reports whether the value holds a lone UTF-16 surrogate.
 *
 * Lone surrogates are not Unicode scalar values, so they cannot exist in Rust
 * input bytes; the strict gate therefore treats them as malformed JSON.
 */
function hasLoneSurrogate(value) {
  for (let index = 0; index < value.length;) {
    const first = value.charCodeAt(index)
    if (first >= 0xdc00 && first <= 0xdfff) return true
    if (first >= 0xd800 && first <= 0xdbff) {
      const second = value.charCodeAt(index + 1)
      if (!(second >= 0xdc00 && second <= 0xdfff)) return true
      index += 2
      continue
    }
    index += 1
  }
  return false
}

/** ECMA-262 whitespace plus U+0085, the set shared with the Rust boundary. */
function isContractWhitespace(character) {
  return /\s/u.test(character) || character === ''
}

function isControlCharacter(codePoint) {
  return codePoint <= 0x1f || (codePoint >= 0x7f && codePoint <= 0x9f)
}

function looksLikeAbsolutePath(value) {
  if (value.startsWith('/')
    || value.startsWith('~/')
    || value.startsWith('file://')
    || value.startsWith('\\\\')) return true
  const first = value.charCodeAt(0)
  const isAsciiLetter = (first >= 0x41 && first <= 0x5a) || (first >= 0x61 && first <= 0x7a)
  return value.charAt(1) === ':'
    && isAsciiLetter
    && (value.charAt(2) === '/' || value.charAt(2) === '\\')
}

/**
 * Applies one shared text rule for source identifiers and display names.
 *
 * Lengths count Unicode code points. Returns `null` when the value is inside
 * the boundary and a `{category, message}` problem otherwise.
 */
function checkText(value, maximumCharacters, label) {
  const characters = [...value]
  if (characters.length === 0) {
    return contentProblem('invalid-content', `${label} is empty`)
  }
  if (characters.length > maximumCharacters) {
    return contentProblem('invalid-content', `${label} exceeds ${maximumCharacters} code points`)
  }
  const first = characters[0]
  const last = characters[characters.length - 1]
  if (isContractWhitespace(first) || isContractWhitespace(last)) {
    return contentProblem('invalid-content', `${label} has leading or trailing whitespace`)
  }
  for (const character of characters) {
    if (isControlCharacter(character.codePointAt(0))) {
      return contentProblem('invalid-content', `${label} contains a control character`)
    }
  }
  if (looksLikeAbsolutePath(value)) {
    return contentProblem('local-path-not-allowed', `${label} looks like a local absolute path`)
  }
  return null
}

function checkSourceId(value, label) {
  return checkText(value, MAXIMUM_SOURCE_ID_CHARACTERS, label)
}

function checkDisplayName(value, label) {
  return checkText(value, MAXIMUM_DISPLAY_NAME_CHARACTERS, label)
}

function checkSlug(value, label) {
  const characters = [...value]
  if (characters.length === 0) {
    return contentProblem('invalid-content', `${label} is empty`)
  }
  if (characters.length > MAXIMUM_SLUG_CHARACTERS) {
    return contentProblem('invalid-content', `${label} exceeds ${MAXIMUM_SLUG_CHARACTERS} code points`)
  }
  const isSlugShape = [...value].every(character => {
    const code = character.charCodeAt(0)
    return (code >= 0x61 && code <= 0x7a) || (code >= 0x30 && code <= 0x39) || code === 0x2d
  })
  if (isSlugShape === false || value.startsWith('-') || value.endsWith('-')) {
    return contentProblem('invalid-content', `${label} is not a lowercase slug`)
  }
  return null
}

function checkExportId(value) {
  const characters = [...value]
  if (characters.length === 0) {
    return contentProblem('invalid-export-id', 'exportId is empty')
  }
  if (characters.length > MAXIMUM_EXPORT_ID_CHARACTERS) {
    return contentProblem(
      'invalid-export-id',
      `exportId exceeds ${MAXIMUM_EXPORT_ID_CHARACTERS} code points`,
    )
  }
  if (EXPORT_ID_PATTERN.test(value) === false) {
    return contentProblem('invalid-export-id', 'exportId has a character outside the allowed set')
  }
  return null
}

function exactMembersProblem(value, names, label) {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    return `${label} must be an object`
  }
  for (const name of names) {
    if (Object.hasOwn(value, name) === false) return `${label} is missing member "${name}"`
  }
  for (const name of Object.keys(value)) {
    if (names.includes(name) === false) return `${label} has unknown member "${name}"`
  }
  return null
}

function stringProblem(value, label) {
  return typeof value === 'string' ? null : `${label} must be a string`
}

function arrayProblem(value, label) {
  return Array.isArray(value) ? null : `${label} must be an array`
}

/** Checks the JSON value shape that serde enforces while decoding the document. */
function structuralProblem(document) {
  let problem = exactMembersProblem(
    document,
    ['format', 'exportId', 'contentSha256', 'content'],
    'document',
  )
  if (problem) return problem
  problem = stringProblem(document.format, 'format')
    ?? stringProblem(document.exportId, 'exportId')
    ?? stringProblem(document.contentSha256, 'contentSha256')
  if (problem) return problem
  problem = exactMembersProblem(
    document.content,
    ['profileDisplayName', 'organizations', 'projects'],
    'content',
  )
  if (problem) return problem
  problem = stringProblem(document.content.profileDisplayName, 'profileDisplayName')
    ?? arrayProblem(document.content.organizations, 'organizations')
    ?? arrayProblem(document.content.projects, 'projects')
  if (problem) return problem
  for (const [index, organization] of document.content.organizations.entries()) {
    problem = exactMembersProblem(
      organization,
      ['sourceOrganizationId', 'slug', 'displayName'],
      `organizations[${index}]`,
    ) ?? stringProblem(organization.sourceOrganizationId, `organizations[${index}].sourceOrganizationId`)
      ?? stringProblem(organization.slug, `organizations[${index}].slug`)
      ?? stringProblem(organization.displayName, `organizations[${index}].displayName`)
    if (problem) return problem
  }
  for (const [index, project] of document.content.projects.entries()) {
    problem = exactMembersProblem(
      project,
      ['sourceProjectId', 'sourceOrganizationId', 'slug', 'displayName'],
      `projects[${index}]`,
    ) ?? stringProblem(project.sourceProjectId, `projects[${index}].sourceProjectId`)
      ?? stringProblem(project.sourceOrganizationId, `projects[${index}].sourceOrganizationId`)
      ?? stringProblem(project.slug, `projects[${index}].slug`)
      ?? stringProblem(project.displayName, `projects[${index}].displayName`)
    if (problem) return problem
  }
  return null
}

function walkLoneSurrogates(value) {
  if (typeof value === 'string') return hasLoneSurrogate(value)
  if (Array.isArray(value)) return value.some(walkLoneSurrogates)
  if (typeof value === 'object' && value !== null) {
    return Object.values(value).some(walkLoneSurrogates)
  }
  return false
}

/** Validates records, references, uniqueness and byte limits for one content value. */
function contentGate(content) {
  const profileProblem = checkDisplayName(content.profileDisplayName, 'profileDisplayName')
  if (profileProblem) return profileProblem
  if (content.organizations.length > MAX_WINWINCODE_EXPORT_ORGANIZATIONS
    || content.projects.length > MAX_WINWINCODE_EXPORT_PROJECTS) {
    return contentProblem('invalid-content', 'content exceeds the record count limits')
  }

  const organizationIds = new Set()
  const organizationSlugs = new Set()
  for (const [index, organization] of content.organizations.entries()) {
    const problem = checkSourceId(organization.sourceOrganizationId, `organizations[${index}].sourceOrganizationId`)
      ?? checkSlug(organization.slug, `organizations[${index}].slug`)
      ?? checkDisplayName(organization.displayName, `organizations[${index}].displayName`)
    if (problem) return problem
    if (organizationIds.has(organization.sourceOrganizationId)
      || organizationSlugs.has(organization.slug)) {
      return contentProblem(
        'duplicate-source-identifier',
        `organizations[${index}] repeats an organization identifier or slug`,
      )
    }
    organizationIds.add(organization.sourceOrganizationId)
    organizationSlugs.add(organization.slug)
  }

  const projectIds = new Set()
  const projectSlugs = new Set()
  for (const [index, project] of content.projects.entries()) {
    const problem = checkSourceId(project.sourceProjectId, `projects[${index}].sourceProjectId`)
      ?? checkSourceId(project.sourceOrganizationId, `projects[${index}].sourceOrganizationId`)
      ?? checkSlug(project.slug, `projects[${index}].slug`)
      ?? checkDisplayName(project.displayName, `projects[${index}].displayName`)
    if (problem) return problem
    if (organizationIds.has(project.sourceOrganizationId) === false) {
      return contentProblem(
        'unknown-source-organization',
        `projects[${index}] references an organization outside this document`,
      )
    }
    const projectSlugKey = `${project.sourceOrganizationId} ${project.slug}`
    if (projectIds.has(project.sourceProjectId) || projectSlugs.has(projectSlugKey)) {
      return contentProblem(
        'duplicate-source-identifier',
        `projects[${index}] repeats a project identifier or an organization slug`,
      )
    }
    projectIds.add(project.sourceProjectId)
    projectSlugs.add(projectSlugKey)
  }
  return null
}

function sameOrganization(left, right) {
  return left.sourceOrganizationId === right.sourceOrganizationId
    && left.slug === right.slug
    && left.displayName === right.displayName
}

function sameProject(left, right) {
  return left.sourceProjectId === right.sourceProjectId
    && left.sourceOrganizationId === right.sourceOrganizationId
    && left.slug === right.slug
    && left.displayName === right.displayName
}

/** Checks that arrays already appear in the canonical scalar-value order. */
function recordOrderProblem(content) {
  const sortedOrganizations = [...content.organizations].sort((left, right) => compareUnicodeScalars(
    left.sourceOrganizationId,
    right.sourceOrganizationId,
  ))
  for (const [index, organization] of content.organizations.entries()) {
    if (sameOrganization(organization, sortedOrganizations[index]) === false) {
      return contentProblem(
        'non-canonical',
        'organizations are not sorted by sourceOrganizationId',
      )
    }
  }
  const sortedProjects = [...content.projects].sort((left, right) => compareUnicodeScalars(
    left.sourceOrganizationId,
    right.sourceOrganizationId,
  ) || compareUnicodeScalars(left.sourceProjectId, right.sourceProjectId))
  for (const [index, project] of content.projects.entries()) {
    if (sameProject(project, sortedProjects[index]) === false) {
      return contentProblem(
        'non-canonical',
        'projects are not sorted by sourceOrganizationId then sourceProjectId',
      )
    }
  }
  return null
}

function digestProblem(document) {
  const declared = document.contentSha256
  const isLowercaseHex = declared.length === 64
    && [...declared].every(character => LOWERCASE_HEX.includes(character))
  if (isLowercaseHex === false) {
    return contentProblem('digest-mismatch', 'contentSha256 is not 64 lowercase hexadecimal digits')
  }
  const actual = createHash('sha256')
    .update(canonicalWinWinCodeExportDigestMaterialBytes(document))
    .digest('hex')
  if (actual !== declared) {
    return contentProblem('digest-mismatch', 'contentSha256 does not match the document content')
  }
  return null
}

function sameBytes(left, right) {
  return left.byteLength === right.byteLength && left.every((byte, index) => byte === right[index])
}

/**
 * Validates one `winwincode-export/v1` value without its serialized spelling.
 *
 * This mirrors `WinWinCodeExport::validate` in the Rust contract owner: record
 * text, record counts, uniqueness, references, canonical record order, and the
 * content digest. Canonical byte spelling needs the original bytes, so use
 * {@link validateWinWinCodeExportBytes} to also reject another serialization
 * of the same JSON value.
 */
export function validateWinWinCodeExportDocument(document) {
  const structural = structuralProblem(document)
  if (structural) return rejection('invalid-json', structural)
  if (walkLoneSurrogates(document)) {
    return rejection('invalid-json', 'document holds a lone UTF-16 surrogate')
  }
  if (document.format !== WINWINCODE_EXPORT_FORMAT) {
    return rejection('unsupported-format', `format must be ${WINWINCODE_EXPORT_FORMAT}`)
  }
  const exportIdProblem = checkExportId(document.exportId)
  if (exportIdProblem) return rejected(exportIdProblem)
  const content = contentGate(document.content)
  if (content) return rejected(content)
  const order = recordOrderProblem(document.content)
  if (order) return rejected(order)
  const digest = digestProblem(document)
  if (digest) return rejected(digest)
  return { status: 'accepted', document, contentSha256: document.contentSha256 }
}

/**
 * Runs the complete strict v1 gate over original document bytes.
 *
 * The gate applies the 16 MiB byte limit, decodes UTF-8 and JSON, enforces the
 * structural and semantic rules of the contract, recomputes `contentSha256`,
 * and finally requires the exact canonical byte spelling. It returns
 * `{status: "accepted", document, contentSha256}` or
 * `{status: "rejected", category, message}` without throwing. The categories
 * match the Rust `WinWinCodeExportError` variants one for one.
 */
export function validateWinWinCodeExportBytes(bytes) {
  if (!(bytes instanceof Uint8Array)) {
    throw new TypeError('WinWinCode export input must be a byte array')
  }
  if (bytes.byteLength > MAX_WINWINCODE_EXPORT_BYTES) {
    return rejection('too-large', 'WinWinCode export exceeds its size limit')
  }
  let document
  try {
    document = parseBoundedWinWinCodeExportJson(bytes)
  } catch (error) {
    if (error instanceof RangeError) return rejection('too-large', error.message)
    return rejection('invalid-json', `WinWinCode export JSON was rejected: ${error.message}`)
  }
  const validated = validateWinWinCodeExportDocument(document)
  if (validated.status === 'rejected') return validated
  const canonical = canonicalWinWinCodeExportBytes(validated.document)
  if (sameBytes(canonical, bytes) === false) {
    return rejection(
      'non-canonical',
      'WinWinCode export bytes or record order are not canonical',
    )
  }
  return validated
}

/** Throws when the strict v1 gate rejects the document bytes. */
export function assertWinWinCodeExportBytes(bytes) {
  const result = validateWinWinCodeExportBytes(bytes)
  if (result.status === 'rejected') {
    const error = new SyntaxError(`${result.category}: ${result.message}`)
    error.category = result.category
    throw error
  }
  return result.document
}
