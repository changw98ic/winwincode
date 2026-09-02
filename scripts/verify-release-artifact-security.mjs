#!/usr/bin/env node

import { createHash } from 'node:crypto'
import {
  existsSync,
  mkdirSync,
  readFileSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { dirname, extname, isAbsolute, relative, resolve, sep } from 'node:path'

import { scanCredentialLeakBytes } from './credential-leak-gate.mjs'
import {
  RELEASE_ARTIFACT_MANIFEST,
  RELEASE_TARGETS,
  canonicalJson,
  createReleaseReport,
  jsonSha256,
  targetConfiguration,
  verifyReleaseArtifactDirectory,
} from './release-artifact-contract.mjs'
import { readCanonicalJson } from './release-source-contract.mjs'

export const RELEASE_ARTIFACT_SECURITY_KIND = 'winwincode.release-artifact-security.v1'

const root = resolve(import.meta.dirname, '..')

const MACH_O_64_MAGIC = 0xfeedfacf
const MACH_O_EXECUTE = 2
const MACH_O_HEADER_BYTES = 32
const MACH_O_DYLIB_COMMANDS = new Set([
  0x0c,
  0x20,
  0x80000018,
  0x8000001f,
  0x80000023,
])
const MACH_O_RPATH_COMMAND = 0x8000001c
const ELF_HEADER_BYTES = 64
const ELF_PROGRAM_HEADER_BYTES = 56
const ELF_PROGRAM_LOAD = 1
const ELF_PROGRAM_DYNAMIC = 2
const ELF_PROGRAM_INTERPRETER = 3
const ELF_DYNAMIC_NEEDED = 1n
const ELF_DYNAMIC_STRING_TABLE = 5n
const ELF_DYNAMIC_STRING_TABLE_BYTES = 10n
const ELF_DYNAMIC_RPATH = 15n
const ELF_DYNAMIC_RUNPATH = 29n
const MAX_BINARY_BYTES = 512 * 1_024 * 1_024

const clientExtensions = new Set([
  '.css',
  '.html',
  '.ico',
  '.js',
  '.json',
  '.png',
  '.svg',
  '.webp',
  '.woff',
  '.woff2',
])

const linuxDynamicLibraryAllowlist = new Set([
  'libanl.so.1',
  'libc.so.6',
  'libdl.so.2',
  'libgcc_s.so.1',
  'libm.so.6',
  'libpthread.so.0',
  'libresolv.so.2',
  'librt.so.1',
  'libutil.so.1',
])

const linuxInterpreterAllowlist = Object.freeze({
  arm64: new Set([
    '/lib/ld-linux-aarch64.so.1',
    '/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1',
  ]),
  x64: new Set([
    '/lib64/ld-linux-x86-64.so.2',
    '/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2',
  ]),
})

const contentRules = Object.freeze([
  Object.freeze({
    id: 'legacy.dsh',
    pattern: /\bDSH\b|DeepSeek Harness|@deepseek-ai\//giu,
    binaryPattern: /\bDSH(?::[A-Z][A-Z0-9_]{2,}\b|\s+(?:conversation|provider|route)\b)|DeepSeek Harness|@deepseek-ai\//gu,
  }),
  Object.freeze({
    id: 'legacy.cordis',
    pattern: /\bCordis\b|\bDshModelPort\b|\bctx\.llm\b/giu,
  }),
  Object.freeze({
    id: 'legacy.native-addon',
    pattern: /\bN-?\x41PI\b|\bn\x61pi(?:-build|-derive)?\b|winwincode_n\x61tive\.node/giu,
  }),
  Object.freeze({
    id: 'legacy.external-executor',
    pattern: /@openai\/c\x6fdex|c\x6fdex-cli\b|installed[- ]c\x6ci|external_fallback\s*[:=]\s*true/giu,
    binaryPattern: /@openai\/c\x6fdex|c\x6fdex-cli\b|installed[- ]c\x6ci|external_fallback\s*[:=]\s*true/gu,
  }),
  Object.freeze({
    id: 'debug.host-path',
    pattern: /(?<![A-Za-z0-9_/\\])(?:file:\/\/)?(?:\/Users\/|\/Volumes\/|\/home\/runner\/work\/|\/private\/tmp\/winwincode-|\/tmp\/winwincode-|[A-Za-z]:\\(?:Users|workspace|runner)\\)/gu,
  }),
  Object.freeze({
    id: 'debug.build-path',
    pattern: /(?:^|[/\\])(?:cargo-primary|cargo-replay|target[/\\]debug)(?:[/\\]|$)/gu,
  }),
])

function fail(message) {
  throw new Error(message)
}

function pathInside(parent, child) {
  const path = relative(parent, child)
  return path === '' || (!path.startsWith(`..${sep}`) && path !== '..' && !isAbsolute(path))
}

function safeNumber(value, label) {
  const number = Number(value)
  if (!Number.isSafeInteger(number) || number < 0) fail(`${label} is outside the safe integer range`)
  return number
}

function zeroTerminated(bytes, start, end, label) {
  if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end)
    || start < 0 || end > bytes.length || start >= end) {
    fail(`${label} string bounds are invalid`)
  }
  const terminator = bytes.indexOf(0, start)
  if (terminator === -1 || terminator >= end) fail(`${label} string is not terminated`)
  return bytes.subarray(start, terminator).toString('utf8')
}

function assertRange(bytes, offset, length, label) {
  if (!Number.isSafeInteger(offset) || !Number.isSafeInteger(length)
    || offset < 0 || length < 0 || offset + length > bytes.length) {
    fail(`${label} exceeds the artifact bounds`)
  }
}

export function inspectMachO64(bytes) {
  if (bytes.length < MACH_O_HEADER_BYTES || bytes.readUInt32LE(0) !== MACH_O_64_MAGIC) {
    fail('artifact is not a thin little-endian 64-bit Mach-O binary')
  }
  if (bytes.readUInt32LE(12) !== MACH_O_EXECUTE) fail('Mach-O artifact is not an executable')
  const cpu = bytes.readUInt32LE(4)
  const arch = cpu === 0x0100000c ? 'arm64' : cpu === 0x01000007 ? 'x64' : null
  if (arch === null) fail(`unsupported Mach-O CPU ${String(cpu)}`)
  const commandCount = bytes.readUInt32LE(16)
  const commandBytes = bytes.readUInt32LE(20)
  assertRange(bytes, MACH_O_HEADER_BYTES, commandBytes, 'Mach-O load commands')
  const dynamicLibraries = []
  const rpaths = []
  let offset = MACH_O_HEADER_BYTES
  for (let index = 0; index < commandCount; index += 1) {
    assertRange(bytes, offset, 8, 'Mach-O load command')
    const command = bytes.readUInt32LE(offset)
    const size = bytes.readUInt32LE(offset + 4)
    if (size < 8) fail('Mach-O load command size is invalid')
    assertRange(bytes, offset, size, 'Mach-O load command')
    if (MACH_O_DYLIB_COMMANDS.has(command)) {
      assertRange(bytes, offset, 24, 'Mach-O dylib command')
      const nameOffset = bytes.readUInt32LE(offset + 8)
      dynamicLibraries.push(zeroTerminated(
        bytes,
        offset + nameOffset,
        offset + size,
        'Mach-O dylib',
      ))
    } else if (command === MACH_O_RPATH_COMMAND) {
      assertRange(bytes, offset, 12, 'Mach-O rpath command')
      const pathOffset = bytes.readUInt32LE(offset + 8)
      rpaths.push(zeroTerminated(
        bytes,
        offset + pathOffset,
        offset + size,
        'Mach-O rpath',
      ))
    }
    offset += size
  }
  if (offset !== MACH_O_HEADER_BYTES + commandBytes) {
    fail('Mach-O load command count and byte length differ')
  }
  return Object.freeze({
    format: 'mach-o-64',
    arch,
    dynamicLibraries: Object.freeze(dynamicLibraries.toSorted()),
    rpaths: Object.freeze(rpaths.toSorted()),
    interpreter: null,
  })
}

function elfProgramHeaders(bytes) {
  const offset = safeNumber(bytes.readBigUInt64LE(32), 'ELF program header offset')
  const size = bytes.readUInt16LE(54)
  const count = bytes.readUInt16LE(56)
  if (size !== ELF_PROGRAM_HEADER_BYTES) fail('ELF program header size is unsupported')
  assertRange(bytes, offset, size * count, 'ELF program headers')
  return Array.from({ length: count }, (_, index) => {
    const start = offset + index * size
    return Object.freeze({
      type: bytes.readUInt32LE(start),
      offset: safeNumber(bytes.readBigUInt64LE(start + 8), 'ELF segment offset'),
      virtualAddress: safeNumber(bytes.readBigUInt64LE(start + 16), 'ELF segment address'),
      fileBytes: safeNumber(bytes.readBigUInt64LE(start + 32), 'ELF segment bytes'),
      memoryBytes: safeNumber(bytes.readBigUInt64LE(start + 40), 'ELF segment memory bytes'),
    })
  })
}

function elfVirtualAddressToOffset(headers, address, length) {
  for (const header of headers) {
    if (header.type !== ELF_PROGRAM_LOAD) continue
    if (address < header.virtualAddress
      || address + length > header.virtualAddress + header.fileBytes) continue
    return header.offset + address - header.virtualAddress
  }
  fail('ELF dynamic string table is not backed by a loadable segment')
}

export function inspectElf64(bytes) {
  if (bytes.length < ELF_HEADER_BYTES
    || !bytes.subarray(0, 4).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46]))
    || bytes[4] !== 2
    || bytes[5] !== 1
    || bytes[6] !== 1) {
    fail('artifact is not a little-endian 64-bit ELF binary')
  }
  if (![2, 3].includes(bytes.readUInt16LE(16))) fail('ELF artifact is not an executable or PIE')
  const machine = bytes.readUInt16LE(18)
  const arch = machine === 183 ? 'arm64' : machine === 62 ? 'x64' : null
  if (arch === null) fail(`unsupported ELF machine ${String(machine)}`)
  const headers = elfProgramHeaders(bytes)
  const dynamic = headers.find(header => header.type === ELF_PROGRAM_DYNAMIC)
  const interpreterHeader = headers.find(header => header.type === ELF_PROGRAM_INTERPRETER)
  const interpreter = interpreterHeader === undefined
    ? null
    : zeroTerminated(
      bytes,
      interpreterHeader.offset,
      interpreterHeader.offset + interpreterHeader.fileBytes,
      'ELF interpreter',
    )
  const neededOffsets = []
  const rpathOffsets = []
  let stringAddress
  let stringBytes
  if (dynamic !== undefined) {
    assertRange(bytes, dynamic.offset, dynamic.fileBytes, 'ELF dynamic segment')
    if (dynamic.fileBytes % 16 !== 0) fail('ELF dynamic segment size is invalid')
    for (let offset = dynamic.offset; offset < dynamic.offset + dynamic.fileBytes; offset += 16) {
      const tag = bytes.readBigInt64LE(offset)
      const value = safeNumber(bytes.readBigUInt64LE(offset + 8), 'ELF dynamic value')
      if (tag === 0n) break
      if (tag === ELF_DYNAMIC_NEEDED) neededOffsets.push(value)
      else if (tag === ELF_DYNAMIC_STRING_TABLE) stringAddress = value
      else if (tag === ELF_DYNAMIC_STRING_TABLE_BYTES) stringBytes = value
      else if (tag === ELF_DYNAMIC_RPATH || tag === ELF_DYNAMIC_RUNPATH) rpathOffsets.push(value)
    }
  }
  if ((neededOffsets.length > 0 || rpathOffsets.length > 0)
    && (!Number.isSafeInteger(stringAddress) || !Number.isSafeInteger(stringBytes))) {
    fail('ELF dynamic strings have no string table')
  }
  const dynamicLibraries = []
  const rpaths = []
  if (Number.isSafeInteger(stringAddress) && Number.isSafeInteger(stringBytes)) {
    const stringOffset = elfVirtualAddressToOffset(headers, stringAddress, stringBytes)
    assertRange(bytes, stringOffset, stringBytes, 'ELF dynamic string table')
    for (const offset of neededOffsets) {
      dynamicLibraries.push(zeroTerminated(
        bytes,
        stringOffset + offset,
        stringOffset + stringBytes,
        'ELF dynamic library',
      ))
    }
    for (const offset of rpathOffsets) {
      rpaths.push(zeroTerminated(
        bytes,
        stringOffset + offset,
        stringOffset + stringBytes,
        'ELF runtime path',
      ))
    }
  }
  return Object.freeze({
    format: 'elf-64',
    arch,
    dynamicLibraries: Object.freeze(dynamicLibraries.toSorted()),
    rpaths: Object.freeze(rpaths.toSorted()),
    interpreter,
  })
}

export function inspectExecutable(bytes, target) {
  return target.os === 'macos' ? inspectMachO64(bytes) : inspectElf64(bytes)
}

export function dynamicLibraryAllowed(target, library) {
  if (target.os === 'macos') {
    return library.startsWith('/usr/lib/')
      || library.startsWith('/System/Library/Frameworks/')
  }
  return linuxDynamicLibraryAllowlist.has(library)
}

function printableAsciiSegments(bytes) {
  const segments = []
  let start = null
  for (let index = 0; index <= bytes.length; index += 1) {
    const printable = index < bytes.length && bytes[index] >= 0x20 && bytes[index] <= 0x7e
    if (printable && start === null) start = index
    if (!printable && start !== null) {
      if (index - start >= 3) segments.push(Object.freeze({ start, bytes: bytes.subarray(start, index) }))
      start = null
    }
  }
  return Object.freeze(segments)
}

function artifactLocation(segmentStart, location) {
  if (location === undefined) return undefined
  const match = /^(?:byte|char):(\d+)$/u.exec(location)
  if (match === null) return location
  return `byte:${String(segmentStart + Number(match[1]))}`
}

function binaryCredentialFinding(rule, index) {
  return Object.freeze({ rule, location: `char:${String(index)}` })
}

function scanBinaryCredentialSegment(bytes, label) {
  const text = bytes.toString('ascii')
  const findings = []
  const directRules = [
    ['text.private-key', /-----BEGIN (?:RSA |OPENSSH )?PRIVATE KEY-----/gu],
    ['text.jwt', /\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b/gu],
    ['text.url-userinfo', /\b(?:https?|wss?):\/\/[^/\s:@]+:[^/\s@]+@/giu],
  ]
  for (const [rule, pattern] of directRules) {
    const match = pattern.exec(text)
    if (match !== null) findings.push(binaryCredentialFinding(rule, match.index))
  }

  // Executables commonly contain adjacent error/help strings and credential-detector regex
  // vocabulary without NUL separators. For opaque token forms, require a decimal digit in the
  // payload. Exact sensitive build inputs are independently matched byte-for-byte below.
  const opaqueRules = [
    ['text.bearer', /\bBearer\s+(?!\[REDACTED\]|<redacted>)([A-Za-z0-9._~+/=-]{12,})/giu],
    ['text.provider-token', /\b((?:sk-[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9_-]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}|xox[baprs]-[A-Za-z0-9-]{10,}|npm_[A-Za-z0-9]{20,}))\b/gu],
    ['text.assignment', /\b(?:api[-_]?key|authorization|client[-_]?secret|password|passwd|private[-_]?key|secret|access[-_]?token|refresh[-_]?token|id[-_]?token|session[-_]?token|token)(?:(?:\s*[=:]\s*["'](?!\[REDACTED\]|<redacted>|redacted\b)([A-Za-z0-9._~+/=-]{8,})["'])|(?:[=:](?!=)(?!\[REDACTED\]|<redacted>|redacted\b)([A-Za-z0-9._~+/=-]{8,})))/giu],
  ]
  for (const [rule, pattern] of opaqueRules) {
    const match = pattern.exec(text)
    const payload = match?.slice(1).find(value => value !== undefined)
    if (match !== null && /[0-9]/u.test(payload) && !/A-Za-z0-9/u.test(payload)) {
      findings.push(binaryCredentialFinding(rule, match.index))
    }
  }

  const basic = /\bBasic\s+([A-Za-z0-9+/]{12,}={0,2})/gu.exec(text)
  if (basic !== null) {
    let decoded = ''
    try {
      decoded = Buffer.from(basic[1], 'base64').toString('utf8')
    } catch {
      decoded = ''
    }
    if (decoded.includes(':')) findings.push(binaryCredentialFinding('text.basic-auth', basic.index))
  }
  return Object.freeze(findings.map(entry => Object.freeze({ label, ...entry })))
}

function scanContentSegment(bytes, segmentStart, target, path) {
  const text = bytes.toString('ascii')
  const findings = []
  for (const rule of contentRules) {
    const pattern = rule.binaryPattern ?? rule.pattern
    const match = pattern.exec(text)
    pattern.lastIndex = 0
    if (match !== null) {
      findings.push({
        target,
        path,
        rule: rule.id,
        location: `byte:${String(segmentStart + match.index)}`,
      })
    }
  }
  for (const entry of scanBinaryCredentialSegment(bytes, path)) {
    const location = artifactLocation(segmentStart, entry.location)
    findings.push({
      target,
      path,
      rule: `credential.${entry.rule}`,
      ...(location === undefined ? {} : { location }),
    })
  }
  return findings
}

export function scanReleaseArtifactContent({
  bytes,
  target,
  path,
  sensitiveInputs = [],
  binary = false,
}) {
  const findings = []
  if (binary) {
    for (const segment of printableAsciiSegments(bytes)) {
      findings.push(...scanContentSegment(segment.bytes, segment.start, target, path))
    }
  } else {
    const text = bytes.toString('utf8')
    for (const rule of contentRules) {
      const match = rule.pattern.exec(text)
      rule.pattern.lastIndex = 0
      if (match !== null) {
        findings.push({ target, path, rule: rule.id, location: `char:${String(match.index)}` })
      }
    }
    const credential = scanCredentialLeakBytes({ bytes, label: path })
    for (const entry of credential.findings) {
      findings.push({
        target,
        path,
        rule: `credential.${entry.rule}`,
        ...(entry.location === undefined ? {} : { location: entry.location }),
      })
    }
  }
  for (const input of sensitiveInputs) {
    const offset = bytes.indexOf(input)
    if (offset !== -1) {
      findings.push({ target, path, rule: 'private-input.exact', location: `byte:${String(offset)}` })
    }
  }
  return Object.freeze(sortedFindings(findings))
}

function sortedFindings(findings) {
  return [...new Map(findings.map(entry => [JSON.stringify(entry), entry])).values()]
    .toSorted((left, right) => JSON.stringify(left).localeCompare(JSON.stringify(right)))
}

function sensitiveInputs(paths) {
  return Object.freeze(paths.map((path, index) => {
    const bytes = readFileSync(path)
    if (bytes.length < 8) fail(`sensitive input ${String(index)} must contain at least 8 bytes`)
    return bytes
  }))
}

export function createReleaseArtifactSecurityReport({
  root,
  evidenceRoot,
  expectedCommit,
  sourceDateEpoch,
  target: requestedTarget = null,
  sensitiveInputPaths = [],
}) {
  const configurations = requestedTarget === null
    ? RELEASE_TARGETS
    : Object.freeze([targetConfiguration(requestedTarget)])
  const releaseReport = requestedTarget === null
    ? createReleaseReport({ root, evidenceRoot, expectedCommit, sourceDateEpoch })
    : null
  const inputs = sensitiveInputs(sensitiveInputPaths)
  const findings = []
  const targets = configurations.map(target => {
    const artifactRoot = resolve(evidenceRoot, target.target)
    const manifest = requestedTarget === null
      ? readCanonicalJson(resolve(artifactRoot, RELEASE_ARTIFACT_MANIFEST))
      : verifyReleaseArtifactDirectory({
        root,
        artifactRoot,
        expectedCommit,
        expectedTarget: target.target,
        expectedSourceDateEpoch: sourceDateEpoch,
      })
    const client = manifest.artifacts.client.files.map(descriptor => {
      const path = resolve(artifactRoot, descriptor.path)
      const bytes = readFileSync(path)
      if (!clientExtensions.has(extname(descriptor.path).toLowerCase())) {
        findings.push({ target: target.target, path: descriptor.path, rule: 'client.file-type' })
      }
      if ((statSync(path).mode & 0o111) !== 0) {
        findings.push({ target: target.target, path: descriptor.path, rule: 'client.executable-mode' })
      }
      findings.push(...scanReleaseArtifactContent({
        bytes,
        target: target.target,
        path: descriptor.path,
        sensitiveInputs: inputs,
        binary: true,
      }))
      return Object.freeze({ path: descriptor.path, bytes: descriptor.bytes, sha256: descriptor.sha256 })
    })
    const rust = manifest.artifacts.rust.map(descriptor => {
      const path = resolve(artifactRoot, descriptor.path)
      const bytes = readFileSync(path)
      if (bytes.length > MAX_BINARY_BYTES) {
        findings.push({ target: target.target, path: descriptor.path, rule: 'binary.maximum-bytes' })
      }
      if ((statSync(path).mode & 0o111) === 0) {
        findings.push({ target: target.target, path: descriptor.path, rule: 'binary.non-executable-mode' })
      }
      let identity
      try {
        identity = inspectExecutable(bytes, target)
      } catch {
        findings.push({ target: target.target, path: descriptor.path, rule: 'binary.identity-invalid' })
        identity = Object.freeze({
          format: 'invalid',
          arch: 'invalid',
          dynamicLibraries: Object.freeze([]),
          rpaths: Object.freeze([]),
          interpreter: null,
        })
      }
      if (identity.arch !== target.arch) {
        findings.push({ target: target.target, path: descriptor.path, rule: 'binary.arch-mismatch' })
      }
      for (const library of identity.dynamicLibraries) {
        if (!dynamicLibraryAllowed(target, library)) {
          findings.push({ target: target.target, path: descriptor.path, rule: 'binary.dynamic-library' })
        }
      }
      if (identity.rpaths.length > 0) {
        findings.push({ target: target.target, path: descriptor.path, rule: 'binary.rpath' })
      }
      if (target.os === 'linux'
        && identity.interpreter !== null
        && !linuxInterpreterAllowlist[target.arch].has(identity.interpreter)) {
        findings.push({ target: target.target, path: descriptor.path, rule: 'binary.interpreter' })
      }
      findings.push(...scanReleaseArtifactContent({
        bytes,
        target: target.target,
        path: descriptor.path,
        sensitiveInputs: inputs,
        binary: true,
      }))
      return Object.freeze({
        packageName: descriptor.packageName,
        binaryName: descriptor.binaryName,
        role: descriptor.role,
        distribution: descriptor.distribution,
        path: descriptor.path,
        bytes: descriptor.bytes,
        sha256: descriptor.sha256,
        format: identity.format,
        arch: identity.arch,
        dynamicLibraries: identity.dynamicLibraries,
        interpreter: identity.interpreter,
      })
    })
    const helperDescriptor = manifest.artifacts.helperReleaseManifest
    const helperPath = resolve(artifactRoot, helperDescriptor.path)
    const helperBytes = readFileSync(helperPath)
    if ((statSync(helperPath).mode & 0o777) !== 0o644) {
      findings.push({
        target: target.target,
        path: helperDescriptor.path,
        rule: 'helper-release.executable-mode',
      })
    }
    findings.push(...scanReleaseArtifactContent({
      bytes: helperBytes,
      target: target.target,
      path: helperDescriptor.path,
      sensitiveInputs: inputs,
    }))
    const helperRelease = readCanonicalJson(helperPath)
    return Object.freeze({
      target: target.target,
      client: Object.freeze(client),
      rust: Object.freeze(rust),
      localComposition: manifest.artifacts.localComposition,
      helperReleaseManifest: Object.freeze({
        path: helperDescriptor.path,
        bytes: helperDescriptor.bytes,
        sha256: helperDescriptor.sha256,
        publicKeyHex: helperDescriptor.publicKeyHex,
        protocol: helperRelease.protocol,
        packageVersion: helperRelease.packageVersion,
        binaryPath: helperRelease.binaryPath,
        binaryMode: helperRelease.binaryMode,
        signatureVerified: true,
        compiledKeyBound: true,
      }),
    })
  })
  const uniqueFindings = sortedFindings(findings)
  const protocols = requestedTarget === null
    ? releaseReport.protocols
    : readCanonicalJson(resolve(
      evidenceRoot,
      requestedTarget,
      RELEASE_ARTIFACT_MANIFEST,
    )).protocols
  const canonicalEvidence = requestedTarget === null
    ? releaseReport
    : readCanonicalJson(resolve(
      evidenceRoot,
      requestedTarget,
      RELEASE_ARTIFACT_MANIFEST,
    ))
  return Object.freeze({
    schemaVersion: 1,
    kind: RELEASE_ARTIFACT_SECURITY_KIND,
    status: uniqueFindings.length === 0 ? 'passed' : 'rejected',
    scope: requestedTarget === null ? 'matrix' : 'target',
    sourceCommit: expectedCommit,
    sourceDateEpoch,
    canonicalEvidenceSha256: jsonSha256(canonicalEvidence),
    protocolSha256: jsonSha256(protocols),
    sensitiveInputCount: inputs.length,
    targets: Object.freeze(targets),
    findings: Object.freeze(uniqueFindings),
  })
}

function parseArguments(argv) {
  const values = new Map()
  const sensitiveInputPaths = []
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (!argument.startsWith('--')) fail(`unexpected argument ${argument}`)
    const separator = argument.indexOf('=')
    const key = argument.slice(2, separator === -1 ? undefined : separator)
    const value = separator === -1 ? argv[index + 1] : argument.slice(separator + 1)
    if (value === undefined || value.startsWith('--')) fail(`${argument} requires a value`)
    if (separator === -1) index += 1
    if (key === 'sensitive-input') sensitiveInputPaths.push(resolve(root, value))
    else {
      if (values.has(key)) fail(`duplicate argument --${key}`)
      values.set(key, value)
    }
  }
  const required = ['expected-commit', 'source-date-epoch', 'evidence', 'output']
  const optional = ['target']
  for (const key of values.keys()) {
    if (!required.includes(key) && !optional.includes(key)) fail(`unknown argument --${key}`)
  }
  for (const key of required) {
    if (!values.has(key)) fail(`--${key} is required`)
  }
  return Object.freeze({
    expectedCommit: values.get('expected-commit'),
    sourceDateEpoch: Number(values.get('source-date-epoch')),
    evidenceRoot: resolve(root, values.get('evidence')),
    output: resolve(root, values.get('output')),
    target: values.get('target') ?? null,
    sensitiveInputPaths: Object.freeze(sensitiveInputPaths),
  })
}

if (process.argv[1] !== undefined && resolve(process.argv[1]) === resolve(import.meta.filename)) {
  const options = parseArguments(process.argv.slice(2))
  if (pathInside(options.evidenceRoot, options.output)) {
    fail('security report output must be outside the release evidence root')
  }
  const report = createReleaseArtifactSecurityReport({ root, ...options })
  const text = canonicalJson(report)
  if (existsSync(options.output) && readFileSync(options.output, 'utf8') !== text) {
    fail(`existing security report differs from current evidence: ${options.output}`)
  }
  mkdirSync(dirname(options.output), { recursive: true })
  writeFileSync(options.output, text)
  process.stdout.write(canonicalJson({
    status: report.status,
    scope: report.scope,
    sourceCommit: report.sourceCommit,
    targets: report.targets.map(entry => entry.target),
    reportSha256: createHash('sha256').update(text).digest('hex'),
    output: options.output,
  }))
  if (report.status !== 'passed') process.exitCode = 1
}
