import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'

import {
  dynamicLibraryAllowed,
  inspectElf64,
  inspectMachO64,
  scanReleaseArtifactContent,
} from '../scripts/verify-release-artifact-security.mjs'

function machO({ cpu = 0x0100000c, library = '/usr/lib/libSystem.B.dylib' } = {}) {
  const name = Buffer.from(`${library}\0`)
  const commandBytes = Math.ceil((24 + name.length) / 8) * 8
  const bytes = Buffer.alloc(32 + commandBytes)
  bytes.writeUInt32LE(0xfeedfacf, 0)
  bytes.writeUInt32LE(cpu, 4)
  bytes.writeUInt32LE(2, 12)
  bytes.writeUInt32LE(1, 16)
  bytes.writeUInt32LE(commandBytes, 20)
  bytes.writeUInt32LE(0x0c, 32)
  bytes.writeUInt32LE(commandBytes, 36)
  bytes.writeUInt32LE(24, 40)
  name.copy(bytes, 56)
  return bytes
}

function elf({ machine = 62, library = 'libc.so.6', rpath = null } = {}) {
  const stringTable = Buffer.from(`\0${library}\0${rpath ?? ''}\0`)
  const programOffset = 64
  const programCount = 2
  const dynamicOffset = programOffset + programCount * 56
  const dynamicBytes = rpath === null ? 64 : 80
  const stringOffset = dynamicOffset + dynamicBytes
  const bytes = Buffer.alloc(stringOffset + stringTable.length)
  Buffer.from([0x7f, 0x45, 0x4c, 0x46]).copy(bytes)
  bytes[4] = 2
  bytes[5] = 1
  bytes[6] = 1
  bytes.writeUInt16LE(3, 16)
  bytes.writeUInt16LE(machine, 18)
  bytes.writeBigUInt64LE(BigInt(programOffset), 32)
  bytes.writeUInt16LE(64, 52)
  bytes.writeUInt16LE(56, 54)
  bytes.writeUInt16LE(programCount, 56)

  bytes.writeUInt32LE(1, programOffset)
  bytes.writeBigUInt64LE(0n, programOffset + 8)
  bytes.writeBigUInt64LE(0n, programOffset + 16)
  bytes.writeBigUInt64LE(BigInt(bytes.length), programOffset + 32)
  bytes.writeBigUInt64LE(BigInt(bytes.length), programOffset + 40)

  const dynamicHeader = programOffset + 56
  bytes.writeUInt32LE(2, dynamicHeader)
  bytes.writeBigUInt64LE(BigInt(dynamicOffset), dynamicHeader + 8)
  bytes.writeBigUInt64LE(BigInt(dynamicOffset), dynamicHeader + 16)
  bytes.writeBigUInt64LE(BigInt(dynamicBytes), dynamicHeader + 32)
  bytes.writeBigUInt64LE(BigInt(dynamicBytes), dynamicHeader + 40)

  bytes.writeBigInt64LE(5n, dynamicOffset)
  bytes.writeBigUInt64LE(BigInt(stringOffset), dynamicOffset + 8)
  bytes.writeBigInt64LE(10n, dynamicOffset + 16)
  bytes.writeBigUInt64LE(BigInt(stringTable.length), dynamicOffset + 24)
  bytes.writeBigInt64LE(1n, dynamicOffset + 32)
  bytes.writeBigUInt64LE(1n, dynamicOffset + 40)
  if (rpath !== null) {
    bytes.writeBigInt64LE(29n, dynamicOffset + 48)
    bytes.writeBigUInt64LE(BigInt(Buffer.byteLength(library) + 2), dynamicOffset + 56)
    bytes.writeBigInt64LE(0n, dynamicOffset + 64)
  } else {
    bytes.writeBigInt64LE(0n, dynamicOffset + 48)
  }
  stringTable.copy(bytes, stringOffset)
  return bytes
}

test('thin Mach-O identity reports the exact CPU and dynamic dependency', () => {
  assert.deepEqual(inspectMachO64(machO()), {
    format: 'mach-o-64',
    arch: 'arm64',
    dynamicLibraries: ['/usr/lib/libSystem.B.dylib'],
    rpaths: [],
    interpreter: null,
  })
  assert.equal(inspectMachO64(machO({ cpu: 0x01000007 })).arch, 'x64')
  assert.throws(() => inspectMachO64(Buffer.from('not-mach-o')))
})

test('ELF identity reports the exact CPU and DT_NEEDED dependency', () => {
  assert.deepEqual(inspectElf64(elf()), {
    format: 'elf-64',
    arch: 'x64',
    dynamicLibraries: ['libc.so.6'],
    rpaths: [],
    interpreter: null,
  })
  assert.equal(inspectElf64(elf({ machine: 183 })).arch, 'arm64')
  assert.deepEqual(inspectElf64(elf({ rpath: '/tmp/injected' })).rpaths, ['/tmp/injected'])
  assert.throws(() => inspectElf64(Buffer.from('not-elf')))
})

test('dynamic library policy accepts system libraries and rejects bundled runtime injection', () => {
  assert.equal(dynamicLibraryAllowed({ os: 'macos' }, '/usr/lib/libSystem.B.dylib'), true)
  assert.equal(dynamicLibraryAllowed(
    { os: 'macos' },
    '/System/Library/Frameworks/Security.framework/Versions/A/Security',
  ), true)
  assert.equal(dynamicLibraryAllowed({ os: 'macos' }, '@rpath/libnode.dylib'), false)
  assert.equal(dynamicLibraryAllowed({ os: 'linux' }, 'libc.so.6'), true)
  assert.equal(dynamicLibraryAllowed({ os: 'linux' }, 'libnode.so.120'), false)
})

test('artifact content scan rejects legacy surfaces, host paths, credentials, and private inputs', () => {
  const privateInput = Buffer.from('private-material-123456')
  const findings = scanReleaseArtifactContent({
    bytes: Buffer.concat([
      Buffer.from(
        'DSH Cordis N-API codex-cli /Volumes/ORICO/work '
        + '/private/tmp/winwincode-release-ABC123/cargo-primary/release '
        + 'token=actualSecret123 ',
      ),
      privateInput,
    ]),
    target: 'aarch64-apple-darwin',
    path: 'bin/winwincode-server',
    sensitiveInputs: [privateInput],
  })
  assert.deepEqual(findings.map(({ rule }) => rule), [
    'legacy.dsh',
    'legacy.cordis',
    'legacy.native-addon',
    'legacy.external-executor',
    'debug.host-path',
    'debug.build-path',
    'credential.text.assignment',
    'private-input.exact',
  ].toSorted())
  assert.deepEqual(scanReleaseArtifactContent({
    bytes: Buffer.from('const tokenMatches=token===expectedToken'),
    target: 'aarch64-apple-darwin',
    path: 'client/client.js',
  }), [])
})

test('binary content scan uses bounded printable strings and reports artifact byte offsets', () => {
  const bytes = Buffer.concat([
    Buffer.from([0xff, 0x00]),
    Buffer.from('DSH route provider'),
    Buffer.from([0x00]),
    Buffer.from('Bearer '),
    Buffer.from([0x00]),
    Buffer.from('abcdefghijklmnop'),
    Buffer.from([0x00]),
    Buffer.from('/private/tmp/winwincode-build/cargo-primary/release'),
  ])
  assert.deepEqual(scanReleaseArtifactContent({
    bytes,
    target: 'aarch64-apple-darwin',
    path: 'bin/winwincode-server',
    binary: true,
  }), [
    {
      target: 'aarch64-apple-darwin',
      path: 'bin/winwincode-server',
      rule: 'debug.build-path',
      location: 'byte:75',
    },
    {
      target: 'aarch64-apple-darwin',
      path: 'bin/winwincode-server',
      rule: 'debug.host-path',
      location: 'byte:46',
    },
    {
      target: 'aarch64-apple-darwin',
      path: 'bin/winwincode-server',
      rule: 'legacy.dsh',
      location: 'byte:2',
    },
  ])
  assert.deepEqual(scanReleaseArtifactContent({
    bytes: Buffer.from('/System/Volumes/Data/Users'),
    target: 'aarch64-apple-darwin',
    path: 'bin/winwincode-server',
    binary: true,
  }), [])
  assert.deepEqual(scanReleaseArtifactContent({
    bytes: Buffer.from([0x90, 0x00, 0x44, 0x53, 0x48, 0x3a, 0x00, 0x90]),
    target: 'aarch64-apple-darwin',
    path: 'bin/winwincode-server',
    binary: true,
  }), [])
  assert.deepEqual(scanReleaseArtifactContent({
    bytes: Buffer.from([0x90, 0x00, ...Buffer.from('codex-clI'), 0x00, 0x90]),
    target: 'x86_64-apple-darwin',
    path: 'bin/winwincode-server',
    binary: true,
  }), [])
  assert.deepEqual(scanReleaseArtifactContent({
    bytes: Buffer.from('codex-cli'),
    target: 'x86_64-apple-darwin',
    path: 'bin/winwincode-server',
    binary: true,
  }), [{
    target: 'x86_64-apple-darwin',
    path: 'bin/winwincode-server',
    rule: 'legacy.external-executor',
    location: 'byte:0',
  }])
  assert.deepEqual(scanReleaseArtifactContent({
    bytes: Buffer.from('[DSH:STREAM_CLOSED]'),
    target: 'aarch64-apple-darwin',
    path: 'bin/winwincode-server',
    binary: true,
  }), [{
    target: 'aarch64-apple-darwin',
    path: 'bin/winwincode-server',
    rule: 'legacy.dsh',
    location: 'byte:1',
  }])
  assert.deepEqual(scanReleaseArtifactContent({
    bytes: Buffer.from('Bearer abcDEF123456789._-'),
    target: 'aarch64-apple-darwin',
    path: 'bin/winwincode-server',
    binary: true,
  }), [{
    target: 'aarch64-apple-darwin',
    path: 'bin/winwincode-server',
    rule: 'credential.text.bearer',
    location: 'byte:0',
  }])
  assert.deepEqual(scanReleaseArtifactContent({
    bytes: Buffer.from(
      'api-key authorization bearer credential password private key secret token '
      + 'sk-A-Za-z0-9_-detector-vocabulary',
    ),
    target: 'aarch64-apple-darwin',
    path: 'bin/winwincode-server',
    binary: true,
  }), [])
})

test('Kernel production sources use only the canonical runtime namespace', () => {
  for (const path of [
    new URL('../crates/kernel/src/lib.rs', import.meta.url),
    new URL('../crates/kernel/src/model_port.rs', import.meta.url),
  ]) {
    const bytes = readFileSync(path)
    const findings = scanReleaseArtifactContent({
      bytes,
      target: 'source',
      path: path.pathname,
    })
    assert.equal(findings.some(({ rule }) => rule === 'legacy.dsh'), false)
  }
  assert.match(
    readFileSync(new URL('../crates/kernel/src/model_port.rs', import.meta.url), 'utf8'),
    /\[WINWINCODE_KERNEL:STREAM_CLOSED\]/u,
  )
})
