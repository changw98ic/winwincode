# SPDX-License-Identifier: Apache-2.0
"""Sidecar-only, one-process debugger control for a real production cut."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import time

POLL = "winwincode_codex::adapter::ProductionCodexAdapter::submit_kernel_turn::{{closure}}"
PROTOCOL = "wwc.submission-intent-debugger.v1"


def digest(path):
    value = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            value.update(block)
    return "sha256:" + value.hexdigest()


def write_json(directory, name, value):
    temporary = directory / (name + ".tmp")
    with open(temporary, "w", encoding="utf8") as target:
        os.chmod(temporary, 0o600)
        json.dump(value, target, indent=2)
        target.write("\n")
    temporary.replace(directory / name)


def verify_source_seal(binary, root, node):
    if root is None or node is None:
        raise RuntimeError("dynamic cuts require explicit workspace and pinned Node paths for official source seal verification")
    source = """
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const root = process.argv[1], directory = process.argv[2];
const { verifyApiProductionSourceSeal } = await import(pathToFileURL(join(root, 'scripts/run-api-production-vertical.mjs')));
const result = verifyApiProductionSourceSeal({ root,
  serverBinary: join(directory, 'winwincode-server'),
  helperExecutable: join(directory, 'winwincode-kernel-helper') });
process.stdout.write(JSON.stringify(result.seal));
"""
    seal = json.loads(subprocess.check_output([
        str(node.resolve(strict=True)), "--input-type=module", "-e", source,
        str(root.resolve(strict=True)), str(binary.parent),
    ], cwd=root, text=True, timeout=60))
    if seal["workerBinarySha256"] != digest(binary):
        raise RuntimeError("official source seal does not bind the selected fresh Worker ELF")
    return {"officialSourceSeal": seal,
            "sourceSealSha256": digest(binary.parent / "winwincode-api-production.source.json")}


def resolve_poll(binary):
    identity = digest(binary)
    def symbols(option):
        output = subprocess.check_output([
            "nm", "--defined-only", "--format=posix", option, str(binary),
        ], text=True)
        return [line.split() for line in output.splitlines()]
    named = [row for row in symbols("--demangle") if row[:1] == [POLL]]
    if len(named) != 1 or named[0][1] not in ("t", "T") or int(named[0][3], 16) <= 0:
        raise RuntimeError("fresh ELF has no unique nonempty async first-poll symbol")
    raw = [row for row in symbols("--no-demangle") if row[1:] == named[0][1:]]
    if len(raw) != 1 or not raw[0][0].startswith("_ZN"):
        raise RuntimeError("fresh ELF poll symbol does not have one exact linkage identity")
    symbol = raw[0][0]
    result = subprocess.run([
        "gdb", "--batch", "--nx", "--nh", str(binary),
        "-ex", "set pagination off", "-ex", "break *'" + symbol + "'",
        "-ex", "python import json; print(json.dumps({'resolvedAddress': int(gdb.parse_and_eval(\"(unsigned long) &'" + symbol + "'\"))}))",
    ], text=True, capture_output=True, check=True)
    if result.stderr.strip():
        raise RuntimeError("fresh ELF symbolic breakpoint preflight returned a diagnostic")
    resolved = json.loads(result.stdout.splitlines()[-1])["resolvedAddress"]
    if resolved != int(named[0][2], 16):
        raise RuntimeError("GDB symbol entry and ELF symbol table disagree")
    assembly = subprocess.check_output([
        "objdump", "-d", "--no-show-raw-insn", "--disassemble=" + symbol, str(binary),
    ], text=True)
    if symbol + ">:" not in assembly:
        raise RuntimeError("the exact fresh poll has no disassembled entry")
    if digest(binary) != identity:
        raise RuntimeError("Worker ELF changed during static symbol preflight")
    return {"protocol": PROTOCOL, "workerSha256": identity, "pollName": POLL,
            "rawPollSymbol": symbol, "symbolSize": int(named[0][3], 16),
            "symbolicEntryVerified": True,
            "pollDisassemblySha256": "sha256:" + hashlib.sha256(assembly.encode()).hexdigest(),
            "disassemblyEntry": assembly.splitlines()[:100]}


def wait_arm(directory):
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        if (directory / "abort.json").exists():
            raise RuntimeError("fixture aborted before debugger arm")
        if (directory / "arm.json").exists():
            return json.loads((directory / "arm.json").read_text())
        time.sleep(0.05)
    raise RuntimeError("fixture did not provide an actual Worker to arm")


def reap_debugger(child):
    if child.poll() is not None:
        return
    for stop in (lambda: child.send_signal(signal.SIGINT), child.terminate, child.kill):
        try:
            stop()
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=5)
            return
        except subprocess.TimeoutExpired:
            continue
    raise RuntimeError("debugger survived SIGINT, SIGTERM and SIGKILL; cleanup is unconfirmed")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--sealed-worker", type=Path, required=True)
    parser.add_argument("--directory", type=Path)
    parser.add_argument("--preflight", action="store_true")
    parser.add_argument("--workspace-root", type=Path)
    parser.add_argument("--node-executable", type=Path)
    args = parser.parse_args()
    binary = args.sealed_worker.resolve(strict=True)
    if args.preflight:
        print(json.dumps(resolve_poll(binary), indent=2))
        return
    # Every dynamic run verifies current source and all official binaries first.
    # The poll symbol is then resolved from that exact verified Worker ELF.
    verified = verify_source_seal(binary, args.workspace_root, args.node_executable)
    selected = {**resolve_poll(binary), **verified}
    directory = args.directory.resolve(strict=True)
    if stat.S_IMODE(directory.stat().st_mode) != 0o700 or not str(directory).startswith("/build/"):
        raise RuntimeError("one isolated private shared /build directory is required")
    if any(directory.iterdir()):
        raise RuntimeError("debugger control directory must be fresh")
    try:
        ready = {**selected, "sidecarPid": os.getpid(),
                 "pidNamespace": os.readlink("/proc/self/ns/pid"),
                 "networkNamespace": os.readlink("/proc/self/ns/net")}
        write_json(directory, "ready.json", ready)
        arm = wait_arm(directory)
        if arm.get("protocol") != PROTOCOL or arm.get("workerSha256") != selected["workerSha256"]:
            raise RuntimeError("fixture request does not bind the exact fresh sealed Worker")
        pid = arm.get("pid")
        if not isinstance(pid, int) or pid <= 1 or digest(f"/proc/{pid}/exe") != selected["workerSha256"]:
            raise RuntimeError("actual Worker executable differs from the fresh sealed ELF")
        write_json(directory, "selection.json", selected)
        environment = {**os.environ, "WWC_SUBMISSION_DEBUGGER_DIRECTORY": str(directory)}
        with open(directory / "gdb.log", "w", encoding="utf8") as output:
            os.chmod(directory / "gdb.log", 0o600)
            child = subprocess.Popen([
                "gdb", "--batch", "--nx", "--nh", "-x",
                str(Path(__file__).with_name("submission-intent-gdb.py")),
            ], env=environment, text=True, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT)
            deadline = time.monotonic() + 90
            try:
                while child.poll() is None:
                    if (directory / "abort.json").exists() or time.monotonic() >= deadline:
                        raise RuntimeError("fixture aborted or debugger exceeded its bounded control window")
                    time.sleep(0.02)
            finally:
                # SIGINT lets the GDB script detach in its finally. Escalation
                # then waits for confirmed tracer exit; unconfirmed cleanup fails.
                reap_debugger(child)
                for status in Path(f"/proc/{pid}/task").glob("*/status"):
                    tracer = next(line.split()[1] for line in status.read_text().splitlines()
                                  if line.startswith("TracerPid:"))
                    if int(tracer) == child.pid:
                        raise RuntimeError("debugger cleanup left an original Worker thread traced")
        if child.returncode != 0 or not (directory / "resumed.json").exists():
            raise RuntimeError("debugger did not complete the one-process cut and detach")
    except Exception as error:
        write_json(directory, "error.json", {"protocol": PROTOCOL, "error": str(error)})
        raise


if __name__ == "__main__":
    main()
