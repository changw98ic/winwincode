# SPDX-License-Identifier: Apache-2.0
"""Executed by GDB in the explicit debugger sidecar, never by production."""
import hashlib
import json
import os
from pathlib import Path
import time

import gdb

DIRECTORY = Path(os.environ["WWC_SUBMISSION_DEBUGGER_DIRECTORY"])
PROTOCOL = "wwc.submission-intent-debugger.v1"


def write_json(name, value):
    temporary = DIRECTORY / (name + ".tmp")
    with open(temporary, "w", encoding="utf8") as target:
        os.chmod(temporary, 0o600)
        json.dump({"protocol": PROTOCOL, **value}, target, indent=2)
        target.write("\n")
    temporary.replace(DIRECTORY / name)


def digest(path):
    value = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            value.update(block)
    return "sha256:" + value.hexdigest()


def all_stopped(pid):
    threads = gdb.selected_inferior().threads()
    if not threads or any(not thread.is_stopped() for thread in threads):
        raise RuntimeError("the real Worker and embedded Core threads are not all stopped")
    result = []
    for thread in threads:
        tid = thread.ptid[1]
        status = Path(f"/proc/{pid}/task/{tid}/status").read_text()
        state = next(line.split()[1] for line in status.splitlines() if line.startswith("State:"))
        if state not in ("t", "T"):
            raise RuntimeError("an original Worker OS thread is still executing")
        result.append({"tid": tid, "state": state})
    return result


def main():
    arm = json.loads((DIRECTORY / "arm.json").read_text())
    selection = json.loads((DIRECTORY / "selection.json").read_text())
    pid = arm["pid"]
    if digest(f"/proc/{pid}/exe") != selection["workerSha256"]:
        raise RuntimeError("actual process executable changed before attach")
    attached = False
    hit = []
    try:
        gdb.execute("set pagination off")
        gdb.execute("set confirm off")
        gdb.execute("set print frame-arguments none")
        gdb.execute("set non-stop off")
        if gdb.parameter("non-stop"):
            raise RuntimeError("debugger all-stop mode is not active")
        gdb.execute(f"file /proc/{pid}/exe")
        gdb.execute(f"attach {pid}")
        attached = True
        if gdb.selected_inferior().pid != pid:
            raise RuntimeError("debugger selected a different process")
        raw = selection["rawPollSymbol"]
        entry = int(gdb.parse_and_eval("(unsigned long) &'" + raw + "'"))
        breakpoint = gdb.Breakpoint("*'" + raw + "'", temporary=True)
        number = breakpoint.number
        if not breakpoint.is_valid() or not breakpoint.enabled:
            raise RuntimeError("the fresh symbolic poll breakpoint was not armed")
        all_stopped(pid)
        write_json("armed.json", {"pid": pid, "requestId": arm["requestId"],
                   "workerSha256": selection["workerSha256"], "rawPollSymbol": raw,
                   "relocatedEntry": hex(entry), "breakpointNumber": number,
                   "allStopMode": True})

        def stopped(event):
            if isinstance(event, gdb.BreakpointEvent):
                hit.append([value.number for value in event.breakpoints])
            else:
                hit.append([])

        gdb.events.stop.connect(stopped)
        gdb.execute("continue")
        gdb.events.stop.disconnect(stopped)
        pc = int(gdb.selected_frame().pc())
        if hit != [[number]] or pc != entry:
            raise RuntimeError("the first stop was not the exact fresh async poll entry")
        frames = []
        frame = gdb.newest_frame()
        while frame is not None and len(frames) < 12:
            frames.append({"function": frame.name(), "pc": hex(frame.pc())})
            frame = frame.older()
        write_json("stopped.json", {"pid": pid, "requestId": arm["requestId"],
                   "debuggerPid": os.getpid(),
                   "workerSha256": selection["workerSha256"], "rawPollSymbol": raw,
                   "relocatedEntry": hex(entry), "pc": hex(pc), "pollHits": 1,
                   "allThreadsStopped": True, "threads": all_stopped(pid),
                   "frames": frames, "stoppedAtMillis": int(time.time() * 1000)})
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if (DIRECTORY / "abort.json").exists():
                raise RuntimeError("fixture aborted at the exact poll cut")
            if (DIRECTORY / "resume.json").exists():
                resume = json.loads((DIRECTORY / "resume.json").read_text())
                if resume.get("requestId") != arm["requestId"]:
                    raise RuntimeError("resume does not belong to this original submission")
                gdb.execute("detach")
                attached = False
                write_json("resumed.json", {"pid": pid, "requestId": arm["requestId"],
                           "pollHits": 1, "detachedAtMillis": int(time.time() * 1000)})
                return
            time.sleep(0.02)
        raise RuntimeError("fixture did not resume within the unchanged lease cut budget")
    finally:
        if attached:
            gdb.execute("detach")


try:
    main()
except BaseException as error:
    write_json("error.json", {"error": str(error)})
    gdb.execute("quit 1")
