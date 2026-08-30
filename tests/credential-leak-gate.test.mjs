import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import test from 'node:test'
import { gzipSync } from 'node:zlib'

import {
  CredentialLeakGateError,
  assertCredentialLeakFreeFile,
  scanCredentialLeakBytes,
  scanCredentialLeakFile,
} from '../scripts/credential-leak-gate.mjs'

const root = resolve(import.meta.dirname, '..')
const fixtures = join(root, 'tests/fixtures/credential-leak-gate')
const arbitrarySecret = Buffer.from('fixture exact value with no recognizable syntax')
const fingerprint = Object.freeze({
  bytes: arbitrarySecret.length,
  sha256: createHash('sha256').update(arbitrarySecret).digest('hex'),
})

function tarGzip(name, contents) {
  const body = Buffer.from(contents)
  const header = Buffer.alloc(512)
  header.write(name, 0, 100, 'utf8')
  header.write('0000600\0', 100, 8, 'ascii')
  header.write('0000000\0', 108, 8, 'ascii')
  header.write('0000000\0', 116, 8, 'ascii')
  header.write(`${body.length.toString(8).padStart(11, '0')}\0`, 124, 12, 'ascii')
  header.write('00000000000\0', 136, 12, 'ascii')
  header.fill(32, 148, 156)
  header[156] = 48
  header.write('ustar\0', 257, 6, 'ascii')
  const checksum = header.reduce((sum, byte) => sum + byte, 0)
  header.write(`${checksum.toString(8).padStart(6, '0')}\0 `, 148, 8, 'ascii')
  const padding = Buffer.alloc(Math.ceil(body.length / 512) * 512 - body.length)
  return gzipSync(Buffer.concat([header, body, padding, Buffer.alloc(1024)]))
}

test('Credential leak fixtures produce deterministic secret-free reports', async () => {
  const safePath = join(fixtures, 'safe-output.json')
  const leakedPath = join(fixtures, 'leaked-output.json')
  const first = scanCredentialLeakFile(safePath, { label: 'safe-output.json' })
  const second = scanCredentialLeakFile(safePath, { label: 'safe-output.json' })
  assert.deepEqual(first, second)
  assert.equal(first.status, 'passed')

  const rejected = scanCredentialLeakFile(leakedPath, { label: 'leaked-output.json' })
  assert.equal(rejected.status, 'rejected')
  assert.equal(rejected.findings.some(entry => entry.rule === 'text.provider-token'), true)
  const leakedBytes = await readFile(leakedPath, 'utf8')
  const secret = leakedBytes.match(/sk-[A-Za-z0-9_-]+/u)?.[0]
  assert.notEqual(secret, undefined)
  assert.equal(JSON.stringify(rejected).includes(secret), false)
  assert.throws(
    () => assertCredentialLeakFreeFile(leakedPath, { label: 'leaked-output.json' }),
    error => error instanceof CredentialLeakGateError
      && error.code === 'CREDENTIAL_LEAK_DETECTED'
      && !error.message.includes(secret),
  )
})

test('exact fingerprints catch arbitrary values across every output kind', () => {
  for (const label of [
    'log.txt',
    'error.txt',
    'debug.txt',
    'serialization.json',
    'event.json',
    'audit.json',
    'artifact.bin',
    'evidence.json',
    'http.json',
    'websocket.json',
    'release-package.bin',
  ]) {
    const report = scanCredentialLeakBytes({
      bytes: Buffer.concat([Buffer.from('safe prefix:'), arbitrarySecret]),
      label,
      fingerprints: [fingerprint],
    })
    assert.equal(report.status, 'rejected', label)
    assert.equal(report.findings.some(entry => entry.rule === 'fingerprint.exact-secret'), true)
    assert.equal(JSON.stringify(report).includes(arbitrarySecret.toString()), false)
  }
})

test('field policy permits references and fails closed for malformed or secret-bearing output', () => {
  const reference = scanCredentialLeakBytes({
    bytes: Buffer.from(JSON.stringify({
      credentialReferenceId: 'crd_00000000000000000000000001',
      credential: 'reference-only',
      secretState: 'revoked',
    })),
    label: 'reference.json',
  })
  assert.equal(reference.status, 'passed')

  for (const bytes of [
    Buffer.from('{"vaultLocator":"local://fixture"}'),
    Buffer.from('{"providerCredential":"fixture-value"}'),
    Buffer.from('{not-json'),
    gzipSync(Buffer.from('not-a-tar')),
  ]) {
    assert.equal(scanCredentialLeakBytes({ bytes, label: 'output.json' }).status, 'rejected')
  }
})

test('release archive scan checks unpacked entries and rejects malformed archives', () => {
  const safe = scanCredentialLeakBytes({
    bytes: tarGzip('package/result.json', '{"credentialReferenceId":"crd_fixture"}'),
    label: 'safe-package.tgz',
  })
  assert.equal(safe.status, 'passed')

  const leaked = scanCredentialLeakBytes({
    bytes: tarGzip(
      'package/result.json',
      '{"summary":"sk-fixturecredentialleakgate1234567890"}',
    ),
    label: 'leaked-package.tgz',
  })
  assert.equal(leaked.status, 'rejected')
  assert.equal(leaked.findings.some(entry => entry.label.includes('!/entry-')), true)
  assert.equal(JSON.stringify(leaked).includes('package/result.json'), false)
})
