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
for arguments in [None, {'files': {'main.py': 'print(1)'}}, {'sourceDirectory':'/tmp'}, {'command':'id'},
                  {'executionLock': '/host/lock'}, {'execution-lock': '/host/lock'}]:
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

test('read-only source identity binds a portable snapshot and rejects unsafe trees', () => {
  const script = fileURLToPath(new URL('../scripts/benchmark-public-smoke.py', import.meta.url))
  const result = spawnSync('python3', ['-I', '-c', `
import pathlib, runpy, sys, tempfile
module = runpy.run_path(sys.argv[1])
identity = module['source_identity']
snapshot = lambda source, destination, config: module['capture_source'](source, config, destination)
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
    expected = {'sha256': 'a5cf80cf281d10c7f4e4e36fef1e1bfb5bd37e392bc46d57835bd788fbd4665b', 'files': 2, 'bytes': 24}
    assert identity(source, config) == expected
    assert snapshot(source, root / 'copy', config) == expected
    assert (root / 'copy/main.py').read_bytes() == b'print(1)'
    assert (root / 'copy/nested/模块.py').read_text() == 'value = "中文"'
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
`, script], { encoding: 'utf8' })
  assert.equal(result.status, 0, result.stderr)
})

test('optional public smoke queue excludes real processes and releases its stable lock on exit', () => {
  const script = fileURLToPath(new URL('../scripts/benchmark-public-smoke.py', import.meta.url))
  const result = spawnSync('python3', ['-I', '-c', `
import os, pathlib, runpy, select, stat, subprocess, sys, tempfile
module = runpy.run_path(sys.argv[1])
slot = module['public_smoke_execution_slot']
child_code = """
import runpy, sys
slot = runpy.run_path(sys.argv[1])['public_smoke_execution_slot']
print('ready', flush=True)
with slot(None if sys.argv[2] == 'none' else sys.argv[2]):
    print('entered', flush=True)
    sys.stdin.readline()
"""
children = []
def start(path):
    child = subprocess.Popen([sys.executable, '-I', '-c', child_code, sys.argv[1], str(path)],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             bufsize=0)
    children.append(child)
    assert child.stdout.readline().strip() == b'ready'
    return child
def entered(child):
    assert select.select([child.stdout], [], [], 5)[0], 'lock waiter did not progress'
    assert child.stdout.readline().strip() == b'entered'
try:
    with tempfile.TemporaryDirectory() as directory:
        root = pathlib.Path(directory)
        lock = root / 'execution.lock'
        owner = start(lock)
        entered(owner)
        original = lock.stat()
        assert stat.S_IMODE(original.st_mode) == 0o600
        waiter = start(lock)
        assert not select.select([waiter.stdout], [], [], 0.15)[0], 'two containers admitted together'
        assert waiter.poll() is None
        independent = start('none')
        entered(independent)
        independent.communicate(b'\\n', timeout=5)
        assert independent.returncode == 0, 'default execution should not acquire a lock'
        owner.terminate()
        owner.wait(timeout=5)
        entered(waiter)
        waiter.communicate(b'\\n', timeout=5)
        assert waiter.returncode == 0
        with slot(lock):
            assert (lock.stat().st_dev, lock.stat().st_ino) == (original.st_dev, original.st_ino)
        for kind in ['symlink', 'public', 'fifo']:
            path = root / kind
            if kind == 'symlink': path.symlink_to(lock)
            elif kind == 'fifo': os.mkfifo(path, 0o600)
            else:
                path.write_text('retained')
                path.chmod(0o644)
            try:
                with slot(path): raise AssertionError('unsafe execution lock accepted')
            except (OSError, ValueError): pass
            if kind == 'public': assert path.read_text() == 'retained' and stat.S_IMODE(path.stat().st_mode) == 0o644
        candidate = root / 'candidate'
        candidate.mkdir()
        linked = root / 'linked-candidate'
        linked.symlink_to(candidate, target_is_directory=True)
        for unsafe in ['relative.lock', candidate / 'lock', linked / 'lock']:
            try: module['trusted_execution_lock'](unsafe, candidate)
            except ValueError: pass
            else: raise AssertionError('candidate-owned execution lock accepted')
        assert module['trusted_execution_lock'](None, candidate) is None
        assert module['trusted_execution_lock'](lock, candidate) == lock.resolve(strict=True)
finally:
    for child in children:
        if child.poll() is None: child.kill()
        child.wait(timeout=5)
`, script], { encoding: 'utf8', timeout: 20_000 })
  assert.equal(result.status, 0, result.stderr)
})

test('queued public smoke captures source before waiting and runs those exact bytes after admission', () => {
  const script = fileURLToPath(new URL('../scripts/benchmark-public-smoke.py', import.meta.url))
  const result = spawnSync('python3', ['-I', '-c', `
import pathlib, runpy, subprocess, sys, tempfile, threading, time, types
from unittest.mock import patch
module = runpy.run_path(sys.argv[1])
with tempfile.TemporaryDirectory() as directory:
    root = pathlib.Path(directory)
    source = root / 'source'
    source.mkdir()
    (source / 'main.py').write_text('original')
    lock = root / 'execution.lock'
    owner = subprocess.Popen([sys.executable, '-I', '-c', """
import runpy, sys
with runpy.run_path(sys.argv[1])['public_smoke_execution_slot'](sys.argv[2]):
    print('held', flush=True)
    sys.stdin.readline()
""", sys.argv[1], str(lock)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        assert owner.stdout.readline().strip() == 'held'
        service = module['PublicSmoke'].__new__(module['PublicSmoke'])
        service.source_directory = source
        service.evidence = root / 'evidence'
        service.evidence.mkdir()
        service.execution_lock = lock
        service.config = {'entry': 'main.py', 'suffixes': ['.py'], 'image': 'frozen-image'}
        service.task_id = 'fixture-task'
        service.cases = [{'input': {}}]
        executions = []
        def execute(snapshot, config, payload):
            executions.append(snapshot)
            assert (snapshot / 'main.py').read_text() == 'original'
            return {'image_id': config['image'], 'submission': module['source_identity'](snapshot, config),
                    'stdout': b'ok', 'stderr': b'', 'returncode': 0, 'reason': 'completed', 'elapsed_seconds': 1}
        service.sandbox = types.SimpleNamespace(execute=execute, score=lambda result, cases: {'total': 1, 'passed': 1})
        results = []
        with patch('subprocess.run', return_value=types.SimpleNamespace(stdout='linux/arm64')):
            thread = threading.Thread(target=lambda: results.append(service.call({})))
            thread.start()
            deadline = time.monotonic() + 5
            while not list(service.evidence.glob('*/source.json')):
                assert thread.is_alive() and time.monotonic() < deadline
                time.sleep(0.01)
            assert executions == [], 'container execution must wait for the shared host lock'
            (source / 'main.py').write_text('edited while waiting')
            owner.communicate('\\n', timeout=5)
            thread.join(timeout=5)
            assert not thread.is_alive()
        assert len(executions) == 1 and results[0]['status'] == 'evaluated'
        assert results[0]['source'] != module['source_identity'](source, service.config)
        assert results[0]['elapsedSeconds'] == 1, 'queue waiting cannot change the grading execution clock'
    finally:
        if owner.poll() is None: owner.kill()
        owner.wait(timeout=5)
`, script], { encoding: 'utf8', timeout: 20_000 })
  assert.equal(result.status, 0, result.stderr)
})
