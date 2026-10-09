#!/usr/bin/env python3
"""Retain the exit status of a benchmark controller, including signal exits."""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess


def retain(directory, name, value):
    destination = directory / name
    temporary = directory / f'.{name}.{os.getpid()}.tmp'
    with temporary.open('x', encoding='utf-8') as handle:
        os.chmod(temporary, 0o600)
        json.dump(value, handle, indent=2)
        handle.write('\n')
    os.replace(temporary, destination)


def timestamp():
    return datetime.now(timezone.utc).isoformat()


def identity(pid):
    return subprocess.run(
        ['ps', '-p', str(pid), '-o', 'lstart='],
        capture_output=True, text=True, check=False,
        env={**os.environ, 'LC_ALL': 'C'},
    ).stdout.strip()


def supervise(directory, node):
    directory = directory.resolve(strict=True)
    controller = directory / 'run.mjs'
    config = json.loads((directory / 'config.json').read_text())
    build = json.loads((directory / 'build-artifacts.json').read_text())
    assert build['cargoBuildRan'] and build['exitCode'] == 0 and build['sourceSnapshotMatches']
    assert json.loads((directory / 'build-progress.json').read_text())['phase'] == 'complete'
    for artifact in build['artifacts']:
        assert hashlib.sha256(Path(artifact['path']).read_bytes()).hexdigest() == artifact['sha256']
    # A second supervisor must never create another paid controller.
    with (directory / 'supervisor-claim.json').open('x', encoding='utf-8') as handle:
        os.chmod(handle.fileno(), 0o600)
        json.dump({'pid': os.getpid(), 'processStartedAt': identity(os.getpid()),
                   'claimedAt': timestamp()}, handle)
    with (directory / 'controller.log').open('xb') as log:
        os.chmod(log.fileno(), 0o600)
        child = subprocess.Popen(
            [node, str(controller)], cwd=config['frozenSource'],
            stdin=subprocess.DEVNULL, stdout=log, stderr=log,
            start_new_session=True,
        )
        launch = {
            'experimentId': config['experimentId'], 'pid': child.pid,
            'processStartedAt': identity(child.pid), 'launchedAt': timestamp(),
            'controller': str(controller),
            'controllerSha256': hashlib.sha256(controller.read_bytes()).hexdigest(),
            'configurationSha256': hashlib.sha256((directory / 'config.json').read_bytes()).hexdigest(),
            'log': str(directory / 'controller.log'), 'supervisorPid': os.getpid(),
            'newBuildVerified': True,
        }
        retain(directory, 'launch.json', launch)
        status = child.wait()
    retain(directory, 'controller-process-exit.json', {
        **launch, 'exitedAt': timestamp(), 'returncode': status,
        'signal': signal.Signals(-status).name if status < 0 else None,
        'controllerFinalRecordPresent': (directory / 'exit.json').exists(),
        'batchResultPresent': (directory / 'batch-result.json').exists(),
    })
    return 0


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    parser.add_argument('--node', default='node')
    arguments = parser.parse_args()
    raise SystemExit(supervise(arguments.directory, arguments.node))
