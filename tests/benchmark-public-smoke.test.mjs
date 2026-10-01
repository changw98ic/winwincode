// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { test } from 'node:test'
import { fileURLToPath } from 'node:url'
import { createECDH, createDecipheriv, hkdfSync } from 'node:crypto'
import { installDevicePublicSmoke } from '../scripts/device-production-fixture.mjs'

test('Device public smoke uses encrypted save then discovery and rejects failed connections', async () => {
  for (const outcome of ['tested', 'connection_failed']) {
    const device = createECDH('prime256v1')
    device.generateKeys()
    let revision = 0
    let mutation
    const operations = []
    const snapshot = () => ({
      clientNodeId: 'device-smoke', revision, encryptionPublicKey: device.getPublicKey().toString('base64'),
      mcpServers: [{ id: 'benchmark_public_smoke', enabled: true, connectionStatus: 'ready', toolNames: ['public_smoke'] }],
    })
    const configuration = { command: '/usr/bin/python3', args: ['-I', '/trusted/public-smoke.py'] }
    const api = { async request(path, options) {
      if (options?.method === 'POST') {
        const envelope = options.body
        assert.equal(envelope.expectedRevision, revision)
        const context = 'winwincode.device-extensions.v1'
        const aad = `${context}\n${envelope.clientNodeId}\n${envelope.requestId}\n${revision}`
        const shared = device.computeSecret(Buffer.from(envelope.publicKey, 'base64'))
        const key = hkdfSync('sha256', shared, Buffer.from(context), Buffer.from(aad), 32)
        const cipher = createDecipheriv('aes-256-gcm', key, Buffer.from(envelope.nonce, 'base64'))
        const bytes = Buffer.from(envelope.ciphertext, 'base64')
        cipher.setAAD(Buffer.from(aad))
        cipher.setAuthTag(bytes.subarray(-16))
        mutation = JSON.parse(Buffer.concat([cipher.update(bytes.subarray(0, -16)), cipher.final()]).toString())
        operations.push(mutation.operation)
        if (mutation.operation === 'save_mcp') assert.deepEqual(JSON.parse(mutation.configuration), configuration)
        revision += 1
        return { status: 202 }
      }
      return { status: 200, json: { online: true, snapshot: snapshot(),
        ...(path.includes('/receipts/') ? { receipt: { outcome: mutation.operation === 'save_mcp' ? 'saved' : outcome } } : {}),
      } }
    } }
    const action = installDevicePublicSmoke({ api, publicClientId: 'client-smoke', configuration })
    if (outcome === 'tested') assert.deepEqual((await action).toolNames, ['public_smoke'])
    else await assert.rejects(action, /configuration failed/u)
    assert.deepEqual(operations, ['save_mcp', 'test_mcp'])
  }
})

test('public smoke rejects host controls and unsafe source paths before execution', () => {
  const script = fileURLToPath(new URL('../scripts/benchmark-public-smoke.py', import.meta.url))
  const result = spawnSync('python3', ['-I', '-c', `
import runpy, sys
m = runpy.run_path(sys.argv[1])
validate = m['validate_checkout_arguments']
validate({})
good = {}
for arguments in [None, {'files': {'main.py': 'print(1)'}}, {'sourceDirectory':'/tmp'}, {'command':'id'}]:
    try: validate(arguments)
    except ValueError: pass
    else: raise AssertionError('model supplied host controls accepted')
dispatch = m['dispatch']
class Service:
    def call(self, arguments):
        assert arguments == good
        return {'status': 'evaluated', 'formalBenchmark': False, 'publicResult': {'score': 0}}
service = Service()
request = {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call',
           'params': {'name': 'public_smoke', 'arguments': good}}
assert dispatch(service, request)['result']['isError'] is False
request['params']['command'] = 'id'
assert dispatch(service, request)['error']['code'] == -32601
assert dispatch(service, {'jsonrpc': '2.0', 'method': 'notifications/initialized'}) is None
tools = dispatch(service, {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'})['result']['tools']
assert [tool['name'] for tool in tools] == ['public_smoke']
assert tools[0]['inputSchema']['additionalProperties'] is False
import tempfile, pathlib, types, json, hashlib
with tempfile.TemporaryDirectory() as directory:
    root = pathlib.Path(directory)
    source = root / 'candidate'
    source.mkdir()
    (source / 'main.py').write_text('original')
    service = m['PublicSmoke'].__new__(m['PublicSmoke'])
    service.evidence = root / 'host'
    attempt = service.evidence / 'attempt'
    (attempt / 'source').mkdir(parents=True)
    (attempt / 'source/main.py').write_text('original')
    def snapshot(path, destination, config):
        return m['source_identity'](path, config)
    def score(result, cases):
        return {'total': 1, 'passed': int(result['stdout'] == b'correct' and result['returncode'] == 0)}
    service.sandbox = types.SimpleNamespace(snapshot=snapshot, strict_json=json.loads, score=score)
    service.config = {'image': 'fixed-image', 'suffixes': ['.py'], 'entry': 'main.py'}
    service.task_id = 'fixed-task'
    service.cases = [{}]
    report = {'status': 'evaluated', 'source': snapshot(source, None, service.config),
              'taskId': service.task_id, 'taskRevision': m['FROZEN_REVISION'],
              'imageId': service.config['image'], 'platform': 'linux/arm64',
              'formalBenchmark': False, 'gradeScope': 'public_examples',
              'returncode': 0, 'publicResult': {'total': 1, 'passed': 1}}
    (attempt / 'result.json').write_text(json.dumps(report))
    (attempt / 'stdout.bin').write_bytes(b'correct')
    (attempt / 'stderr.bin').write_bytes(b'')
    from unittest.mock import patch
    with patch('tempfile.TemporaryDirectory', side_effect=AssertionError('verification must be read-only')):
        assert service.verify(source)['verified'] is True
    for target, replacement in [(source / 'main.py', b'changed'),
                                (attempt / 'source/main.py', b'changed'),
                                (attempt / 'stdout.bin', b'wrong')]:
        previous = target.read_bytes()
        target.write_bytes(replacement)
        try:
            service.verify(source)
        except ValueError:
            pass
        else:
            raise AssertionError('unbound or invalid result accepted')
        target.write_bytes(previous)
    service.evidence = source
    try:
        service.verify(source)
    except ValueError:
        pass
    else:
        raise AssertionError('candidate-owned receipts accepted')
`, script], { encoding: 'utf8' })
  assert.equal(result.status, 0, result.stderr)
})

test('read-only source identity matches the frozen snapshot and rejects unsafe trees', () => {
  const script = fileURLToPath(new URL('../scripts/benchmark-public-smoke.py', import.meta.url))
  const frozen = fileURLToPath(new URL('../fusion-benchmark-tasks/agent-benchmark-tasks/tools/sandbox.py', import.meta.url))
  const result = spawnSync('python3', ['-I', '-c', `
import pathlib, runpy, sys, tempfile
identity = runpy.run_path(sys.argv[1])['source_identity']
snapshot = runpy.run_path(sys.argv[2])['snapshot']
config = {'entry': 'main.py', 'suffixes': ['.py']}
with tempfile.TemporaryDirectory() as directory:
    root = pathlib.Path(directory)
    source = root / 'source'
    source.mkdir()
    (source / 'main.py').write_text('print(1)')
    (source / 'nested').mkdir()
    (source / 'nested/模块.py').write_text('value = "中文"')
    (source / 'TASK.md').write_text('ignored non-source')
    (source / '__pycache__').mkdir()
    (source / '__pycache__/ignored.py').write_text('ignored')
    assert identity(source, config) == snapshot(source, root / 'copy', config)
    for name, kind in [('link.py', 'symlink'), ('link-dir', 'directory'),
                       ('large.py', 'large'), ('pipe.py', 'pipe')]:
        path = source / name
        if kind == 'symlink': path.symlink_to(source / 'main.py')
        elif kind == 'directory': path.symlink_to(source / 'nested', target_is_directory=True)
        elif kind == 'large': path.write_bytes(b'x' * 2097153)
        else:
            import os
            os.mkfifo(path)
        for read in [lambda: identity(source, config),
                     lambda: snapshot(source, root / name, config)]:
            try: read()
            except ValueError: pass
            else: raise AssertionError('unsafe tree accepted: ' + kind)
        path.unlink()
    (source / 'main.py').unlink()
    try: identity(source, config)
    except ValueError: pass
    else: raise AssertionError('missing entry accepted')
`, script, frozen], { encoding: 'utf8' })
  assert.equal(result.status, 0, result.stderr)
})
