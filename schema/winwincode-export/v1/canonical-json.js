// SPDX-License-Identifier: Apache-2.0

import { parseBoundedWinWinCodeExportJson } from './parse-bounded.js'

const utf8Encoder = new TextEncoder()
const shortEscapes = new Map([
  [0x08, '\\b'],
  [0x09, '\\t'],
  [0x0a, '\\n'],
  [0x0c, '\\f'],
  [0x0d, '\\r'],
])

function scalarAt(value, index) {
  const first = value.charCodeAt(index)
  if (first >= 0xd800 && first <= 0xdbff) {
    const second = value.charCodeAt(index + 1)
    if (!(second >= 0xdc00 && second <= 0xdfff)) {
      throw new TypeError('canonical JSON strings must contain Unicode scalar values')
    }
    return {
      scalar: ((first - 0xd800) * 0x400) + second - 0xdc00 + 0x10000,
      width: 2,
    }
  }
  if (first >= 0xdc00 && first <= 0xdfff) {
    throw new TypeError('canonical JSON strings must contain Unicode scalar values')
  }
  return { scalar: first, width: 1 }
}

/** Encodes one string token under the `winwincode-export/v1` canonical JSON rules. */
export function canonicalJsonString(value) {
  if (typeof value !== 'string') throw new TypeError('canonical JSON value must be a string')

  let encoded = '"'
  for (let index = 0; index < value.length;) {
    const { scalar, width } = scalarAt(value, index)
    index += width
    if (scalar === 0x22) encoded += '\\"'
    else if (scalar === 0x5c) encoded += '\\\\'
    else if (shortEscapes.has(scalar)) encoded += shortEscapes.get(scalar)
    else if (scalar <= 0x1f) encoded += `\\u00${scalar.toString(16).padStart(2, '0')}`
    else encoded += String.fromCodePoint(scalar)
  }
  return `${encoded}"`
}

/** Compares strings by Unicode scalar value, independently of JavaScript's UTF-16 ordering. */
export function compareUnicodeScalars(left, right) {
  let leftIndex = 0
  let rightIndex = 0
  while (leftIndex < left.length && rightIndex < right.length) {
    const leftValue = scalarAt(left, leftIndex)
    const rightValue = scalarAt(right, rightIndex)
    if (leftValue.scalar !== rightValue.scalar) return leftValue.scalar - rightValue.scalar
    leftIndex += leftValue.width
    rightIndex += rightValue.width
  }
  return (left.length - leftIndex) - (right.length - rightIndex)
}

function canonicalOrganization(organization) {
  return `{"sourceOrganizationId":${canonicalJsonString(organization.sourceOrganizationId)},`
    + `"slug":${canonicalJsonString(organization.slug)},`
    + `"displayName":${canonicalJsonString(organization.displayName)}}`
}

function canonicalProject(project) {
  return `{"sourceProjectId":${canonicalJsonString(project.sourceProjectId)},`
    + `"sourceOrganizationId":${canonicalJsonString(project.sourceOrganizationId)},`
    + `"slug":${canonicalJsonString(project.slug)},`
    + `"displayName":${canonicalJsonString(project.displayName)}}`
}

function canonicalContent(content) {
  const organizations = [...content.organizations]
    .sort((left, right) => compareUnicodeScalars(
      left.sourceOrganizationId,
      right.sourceOrganizationId,
    ))
  const projects = [...content.projects]
    .sort((left, right) => compareUnicodeScalars(
      left.sourceOrganizationId,
      right.sourceOrganizationId,
    ) || compareUnicodeScalars(left.sourceProjectId, right.sourceProjectId))

  return `{"profileDisplayName":${canonicalJsonString(content.profileDisplayName)},`
    + `"organizations":[${organizations.map(canonicalOrganization).join(',')}],`
    + `"projects":[${projects.map(canonicalProject).join(',')}]}`
}

/** Produces the exact bytes hashed for `contentSha256`. */
export function canonicalWinWinCodeExportDigestMaterialBytes(document) {
  const encoded = `{"format":${canonicalJsonString(document.format)},`
    + `"exportId":${canonicalJsonString(document.exportId)},`
    + `"content":${canonicalContent(document.content)}}`
  return utf8Encoder.encode(encoded)
}

/** Produces the exact complete canonical document bytes. */
export function canonicalWinWinCodeExportBytes(document) {
  const encoded = `{"format":${canonicalJsonString(document.format)},`
    + `"exportId":${canonicalJsonString(document.exportId)},`
    + `"contentSha256":${canonicalJsonString(document.contentSha256)},`
    + `"content":${canonicalContent(document.content)}}`
  return utf8Encoder.encode(encoded)
}

/** Applies the byte gate and rejects another serialization of the same JSON value. */
export function decodeCanonicalWinWinCodeExportJson(bytes) {
  const document = parseBoundedWinWinCodeExportJson(bytes)
  const canonical = canonicalWinWinCodeExportBytes(document)
  if (canonical.byteLength !== bytes.byteLength
    || canonical.some((byte, index) => byte !== bytes[index])) {
    throw new SyntaxError('WinWinCode export JSON is not canonical')
  }
  return document
}
