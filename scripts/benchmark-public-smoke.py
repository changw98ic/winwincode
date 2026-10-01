#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Device MCP tool for the frozen benchmark's public examples, never hidden grading."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess
import sys
import uuid

FROZEN_REVISION = 'fa9da301e493fb88d48c86cb8954ed46d9cd2ffe'
MAX_FRAME = 16 * 1024 * 1024
MAX_SOURCE = 2 * 1024 * 1024
TOOL = 'public_smoke'


def write_record(path, value):
    with path.open('x', encoding='utf8') as stream:
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())


def validate_checkout_arguments(arguments):
    if arguments != {}:
        raise ValueError('this tool reads the trusted Session checkout and accepts no arguments')


def capture_source(source, config, destination=None):
    """Bind hashes and execution to the same bounded bytes from opened regular files."""
    source = Path(source).resolve(strict=True)
    if not source.is_dir():
        raise ValueError('source directory missing')
    if destination is not None:
        destination = Path(destination)
        destination.mkdir(mode=0o700)
    files, total = [], 0
    excluded = {'.git', 'node_modules', 'target', 'bin', 'obj', 'build', '.venv', '__pycache__'}
    for parent, dirs, names, directory_fd in os.fwalk(source, follow_symlinks=False):
        dirs[:] = sorted(d for d in dirs if d not in excluded)
        for name in dirs:
            if not stat.S_ISDIR(os.stat(name, dir_fd=directory_fd, follow_symlinks=False).st_mode):
                raise ValueError('symlink directory rejected')
        for name in sorted(names):
            if stat.S_ISLNK(os.stat(name, dir_fd=directory_fd, follow_symlinks=False).st_mode):
                raise ValueError('symlink file rejected')
            if Path(name).suffix not in config['suffixes']:
                continue
            fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory_fd)
            with os.fdopen(fd, 'rb') as stream:
                before = os.fstat(stream.fileno())
                if not stat.S_ISREG(before.st_mode):
                    raise ValueError('non-regular source file')
                if total + before.st_size > MAX_SOURCE or len(files) >= 100:
                    raise ValueError('source limit exceeded')
                data = stream.read(MAX_SOURCE - total + 1)
                after = os.fstat(stream.fileno())
            if len(data) != before.st_size or (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (after.st_size, after.st_mtime_ns, after.st_ctime_ns):
                raise ValueError('source changed during snapshot')
            total += len(data)
            relative = (Path(parent) / name).relative_to(source)
            files.append((relative.as_posix(), hashlib.sha256(data).hexdigest()))
            if destination is not None:
                output = destination / relative
                output.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                with output.open('xb') as stream:
                    stream.write(data)
    if config['entry'] not in {name for name, _ in files}:
        raise ValueError('required entry missing')
    digest = hashlib.sha256(json.dumps(sorted(files), separators=(',', ':')).encode()).hexdigest()
    return {'sha256': digest, 'files': len(files), 'bytes': total}


def source_identity(source, config):
    return capture_source(source, config)


class PublicSmoke:
    def __init__(self, task_root, task_id, image_id, evidence_directory):
        self.root = Path(task_root).resolve(strict=True)
        revision = subprocess.run(['git', '-C', str(self.root), 'rev-parse', 'HEAD'],
                                  capture_output=True, text=True, check=True).stdout.strip()
        if revision != FROZEN_REVISION:
            raise ValueError('task revision mismatch')
        subprocess.run(['git', '-C', str(self.root), 'diff', '--quiet', 'HEAD', '--'], check=True)
        if not re.fullmatch(r'sha256:[0-9a-f]{64}', image_id):
            raise ValueError('immutable image identity required')
        spec = importlib.util.spec_from_file_location('frozen_public_sandbox', self.root / 'tools/sandbox.py')
        self.sandbox = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.sandbox)
        catalog = self.sandbox.strict_json((self.root / 'catalog.json').read_text())
        task = next(item for item in catalog if item['id'] == task_id)
        self.config = self.sandbox.strict_json((self.root / 'environments/manifest.json').read_text())[task['environment']]
        self.config = dict(self.config, image=image_id)
        self.cases = self.sandbox.strict_json((self.root / 'tasks' / task_id / 'examples.json').read_text())
        self.source_directory = Path.cwd().resolve(strict=True)
        self.task_id = task_id
        self.evidence = Path(evidence_directory).resolve()
        if self.evidence == self.source_directory or self.source_directory in self.evidence.parents:
            raise ValueError('host evidence must be outside the candidate tree')
        self.evidence.mkdir(parents=True, exist_ok=True, mode=0o700)

    def verify(self, source_directory):
        """Read host-owned receipts from outside the candidate's writable tree."""
        source_directory = Path(source_directory).resolve(strict=True)
        if self.evidence == source_directory or source_directory in self.evidence.parents:
            raise ValueError('host evidence must be outside the candidate tree')
        source = source_identity(source_directory, self.config)
        for attempt in sorted(self.evidence.iterdir()):
            if not attempt.is_dir() or attempt.is_symlink():
                continue
            try:
                report = self.sandbox.strict_json((attempt / 'result.json').read_text())
                if (report.get('status') != 'evaluated' or report.get('source') != source
                        or report.get('taskId') != self.task_id
                        or report.get('taskRevision') != FROZEN_REVISION
                        or report.get('imageId') != self.config['image']
                        or report.get('platform') != 'linux/arm64'
                        or report.get('formalBenchmark') is not False
                        or report.get('gradeScope') != 'public_examples'):
                    continue
                frozen = source_identity(attempt / 'source', self.config)
                if frozen != source:
                    continue
                result = dict(report, stdout=(attempt / 'stdout.bin').read_bytes(),
                              stderr=(attempt / 'stderr.bin').read_bytes())
                score = self.sandbox.score(result, self.cases)
                if score['total'] > 0 and score['passed'] == score['total'] and score == report['publicResult']:
                    return {'verified': True, 'formalBenchmark': False, 'gradeScope': 'public_examples',
                            'taskId': self.task_id, 'attemptId': attempt.name, 'source': source}
            except (OSError, ValueError, KeyError, TypeError):
                continue
        raise ValueError('no passing host receipt matches the candidate source')

    def call(self, arguments):
        attempt = self.evidence / uuid.uuid4().hex
        attempt.mkdir(mode=0o700)
        identity = {'taskId': self.task_id, 'taskRevision': FROZEN_REVISION,
                    'imageId': self.config['image'], 'formalBenchmark': False,
                    'gradeScope': 'public_examples', 'attemptId': attempt.name}
        write_record(attempt / 'started.json', dict(identity, status='started'))
        try:
            validate_checkout_arguments(arguments)
            source = capture_source(self.source_directory, self.config, attempt / 'source')
            write_record(attempt / 'source.json', source)
            platform = subprocess.run(['docker', 'image', 'inspect', self.config['image'],
                                       '--format', '{{.Os}}/{{.Architecture}}'],
                                      capture_output=True, text=True, check=True).stdout.strip()
            if platform != 'linux/arm64':
                raise ValueError('frozen grading platform mismatch')
            payload = ''.join(json.dumps(case['input'], ensure_ascii=False) + '\n' for case in self.cases).encode()
            result = self.sandbox.execute(attempt / 'source', self.config, payload)
            if result['image_id'] != self.config['image'] or result['submission'] != source:
                raise ValueError('grading identity mismatch')
            (attempt / 'stdout.bin').write_bytes(result['stdout'])
            (attempt / 'stderr.bin').write_bytes(result['stderr'])
            report = dict(identity, status='evaluated', source=source, platform=platform,
                          returncode=result['returncode'], reason=result['reason'],
                          elapsedSeconds=result['elapsed_seconds'],
                          publicResult=self.sandbox.score(result, self.cases),
                          stderr=result['stderr'][-16384:].decode('utf8', 'replace'))
            write_record(attempt / 'result.json', report)
            return report
        except Exception as error:
            # Host errors must not disclose credentials or arbitrary host diagnostics.
            report = dict(identity, status='failed', errorType=type(error).__name__)
            write_record(attempt / 'result.json', report)
            return report


def dispatch(service, request):
    if not isinstance(request, dict) or request.get('jsonrpc') != '2.0':
        raise ValueError('invalid JSON-RPC request')
    if 'id' not in request:
        return None
    method = request.get('method')
    params = request.get('params', {})
    if not isinstance(params, dict):
        raise ValueError('invalid params')
    if method == 'initialize':
        result = {'protocolVersion': '2024-11-05', 'capabilities': {'tools': {}},
                  'serverInfo': {'name': 'winwincode-public-smoke', 'version': '1.0.0'}}
    elif method == 'ping':
        result = {}
    elif method == 'tools/list':
        result = {'tools': [{'name': TOOL,
                            'description': 'Run the fixed public examples against the trusted Session checkout. Save source files before calling. No arguments are accepted.',
                            'inputSchema': {'type': 'object', 'additionalProperties': False,
                                            'properties': {}}}]}
    elif method == 'tools/call' and params.get('name') == TOOL and set(params) <= {'name', 'arguments', '_meta'}:
        report = service.call(params.get('arguments'))
        result = {'content': [{'type': 'text', 'text': json.dumps(report, ensure_ascii=False)}],
                  'isError': report['status'] != 'evaluated'}
    else:
        return {'jsonrpc': '2.0', 'id': request['id'], 'error': {'code': -32601, 'message': 'Method not found'}}
    return {'jsonrpc': '2.0', 'id': request['id'], 'result': result}


def main():
    if not sys.flags.isolated:
        raise ValueError('launch with python -I')
    allowed = {'PATH', 'HOME', 'USER', 'LOGNAME', 'TMPDIR', 'TMP', 'TEMP', 'LANG', 'LC_ALL', 'TZ'}
    for name in list(os.environ):
        if name not in allowed:
            del os.environ[name]
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--task-root', required=True)
    parser.add_argument('--task-id', required=True)
    parser.add_argument('--image-id', required=True)
    parser.add_argument('--evidence-directory', required=True)
    parser.add_argument('--run-source', choices=['.'], help='Execute and verify the current trusted checkout')
    parser.add_argument('--verify-source', help='Verify host public-test evidence against this read-only candidate')
    args = parser.parse_args()
    service = PublicSmoke(args.task_root, args.task_id, args.image_id, args.evidence_directory)
    if args.run_source:
        report = service.call({})
        if report['status'] != 'evaluated':
            raise ValueError('public execution did not complete')
        print(json.dumps(service.verify(service.source_directory), ensure_ascii=False))
        return
    if args.verify_source:
        print(json.dumps(service.verify(args.verify_source), ensure_ascii=False))
        return
    while True:
        line = sys.stdin.buffer.readline(MAX_FRAME + 1)
        if not line:
            break
        if len(line) > MAX_FRAME:
            break
        request = None
        try:
            request = service.sandbox.strict_json(line.decode('utf8'))
            response = dispatch(service, request)
        except Exception:
            response = {'jsonrpc': '2.0', 'id': request.get('id') if isinstance(request, dict) else None,
                        'error': {'code': -32602, 'message': 'Invalid request'}}
        if response is not None:
            print(json.dumps(response, ensure_ascii=False), flush=True)


if __name__ == '__main__':
    main()
