#!/usr/bin/env python3

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import shlex
import signal
import stat
import subprocess
import sys
import time

MAX_IDLE_SECONDS = 24 * 60 * 60
SCHEMA = "winwincode-rust-build-v1"


def directory(path):
    path.mkdir(exist_ok=True)
    if not stat.S_ISDIR(path.lstat().st_mode):
        raise ValueError(f"cache directory must not be a symlink: {path}")
    return path


def regular_open(path, flags):
    fd = os.open(path, flags | os.O_NOFOLLOW, 0o600)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise ValueError(f"cache file must be regular: {path}")
    return fd


def read_record(path):
    with os.fdopen(regular_open(path, os.O_RDONLY)) as stream:
        return json.load(stream)


def write_record(path, record):
    temporary = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    with os.fdopen(regular_open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL), "w") as stream:
        json.dump(record, stream, sort_keys=True)
    os.replace(temporary, path)


def cache_root(root):
    base = directory(directory(root / ".cache") / "rust-build-v1")
    directory(base / "locks")
    for profile in ("debug", "release"):
        directory(base / profile)
    return base


def lock_entry(base, profile, key, blocking=True):
    fd = regular_open(base / "locks" / f"{profile}-{key}.lock", os.O_RDWR | os.O_CREAT)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
    except BlockingIOError:
        os.close(fd)
        return None
    return fd


def valid_record(record, root, profile, key):
    return (record.get("schema") == SCHEMA and record.get("project") == str(root)
            and record.get("profile") == profile and record.get("key") == key
            and type(record.get("last_used")) in (int, float)
            and 0 <= record["last_used"] < float("inf"))


def cleanup(root, now=None):
    base = cache_root(root)
    now = time.time() if now is None else now
    for profile in ("debug", "release"):
        for entry in (base / profile).iterdir():
            if not re.fullmatch(r"[a-f0-9]{64}", entry.name) or not stat.S_ISDIR(entry.lstat().st_mode):
                continue
            fd = lock_entry(base, profile, entry.name, blocking=False)
            if fd is None:
                continue
            try:
                if not entry.exists() or not stat.S_ISDIR(entry.lstat().st_mode):
                    continue
                record = read_record(entry / "usage.json")
                if valid_record(record, root, profile, entry.name) and now - record["last_used"] >= MAX_IDLE_SECONDS:
                    shutil.rmtree(entry)
                    print(f"removed idle Rust {profile} cache {entry.name}", file=sys.stderr)
            except (OSError, ValueError) as error:
                print(f"Rust cache cleanup skipped {entry.name}: {error}", file=sys.stderr)
            finally:
                os.close(fd)


def compatibility_key(root, profile, variant, command, environment):
    digest = hashlib.sha256()
    arguments = [] if len(command) > 1 and Path(command[1]).name in ("test-rust-workspace.mjs", "rust-test-archive.mjs") else command
    if variant == "default-features" and command[:2] == ["cargo", "build"]:
        arguments = []
        skip = False
        for argument in command:
            if skip:
                skip = False
            elif argument in ("-p", "--package", "--bin"):
                skip = True
            elif argument not in ("--locked", "--offline"):
                arguments.append(argument)
    values = [SCHEMA, str(root), platform.system(), platform.machine(), platform.release(), profile, variant, arguments]
    for executable, args in ((environment.get("RUSTC", "rustc"), ["-Vv"]), ("cargo", ["-V"])):
        values.append(subprocess.check_output([executable, *args], cwd=root, env=environment, text=True))
    for name in ("CC", "CXX", "CLANG_PATH"):
        if environment.get(name):
            values.append(subprocess.check_output([*shlex.split(environment[name]), "--version"], cwd=root, env=environment, text=True))
    values.append(sorted((key, value) for key, value in environment.items()
                         if re.match(r"^(CARGO_|RUST|CC$|CXX$|CFLAGS|CXXFLAGS|CPPFLAGS|LDFLAGS|CLANG|LLVM|V8_|GN_|SCCACHE_C_CUSTOM_CACHE_BUSTER)", key)))
    configs = [root / "Cargo.lock", root / "Cargo.toml", root / "rust-toolchain.toml"]
    for parent in [root, *root.parents]:
        configs.extend(parent / ".cargo" / name for name in ("config", "config.toml"))
    cargo_home = Path(environment.get("CARGO_HOME", str(Path.home() / ".cargo")))
    configs.extend(cargo_home / name for name in ("config", "config.toml"))
    for path in configs:
        if path.is_file():
            digest.update(str(path).encode())
            digest.update(path.read_bytes())
    digest.update(json.dumps(values, sort_keys=True).encode())
    return digest.hexdigest()


def run_cached(root, profile, variant, command, environment=None):
    root = root.resolve()
    environment = dict(os.environ if environment is None else environment)
    if environment.get("WWC_RUST_CACHE") == "off" or environment.get("CARGO_BUILD_BUILD_DIR"):
        return subprocess.call(command, cwd=root, env=environment)
    key = compatibility_key(root, profile, variant, command, environment)
    base = cache_root(root)
    fd = lock_entry(base, profile, key)
    try:
        entry = base / profile / key
        existed = entry.exists()
        directory(entry)
        record = {"schema": SCHEMA, "project": str(root), "profile": profile, "key": key, "last_used": time.time()}
        if existed and not valid_record(read_record(entry / "usage.json"), root, profile, key):
            raise ValueError("refusing an unowned Rust build cache")
        write_record(entry / "usage.json", record)
        build = directory(entry / "build")
    except BaseException:
        os.close(fd)
        raise
    environment["CARGO_BUILD_BUILD_DIR"] = str(build)
    print(f"using Rust {profile} cache {key}", file=sys.stderr)
    result = 1
    child = None

    def forward(signum, _frame):
        if child is not None:
            os.killpg(child.pid, signum)

    previous = {signum: signal.signal(signum, forward) for signum in (signal.SIGINT, signal.SIGTERM)}
    try:
        child = subprocess.Popen(command, cwd=root, env=environment, pass_fds=(fd,), start_new_session=True)
        result = child.wait()
        if result < 0:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    finally:
        for signum, handler in previous.items():
            signal.signal(signum, handler)
        try:
            record["last_used"] = time.time()
            write_record(entry / "usage.json", record)
        except (OSError, ValueError) as error:
            print(f"Rust cache usage update failed: {error}", file=sys.stderr)
        os.close(fd)
        try:
            cleanup(root)
        except (OSError, ValueError) as error:
            print(f"Rust cache cleanup failed: {error}", file=sys.stderr)
    return result if result >= 0 else 128 - result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile", choices=("debug", "release"), default="debug")
    parser.add_argument("--variant", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a build command is required")
    return run_cached(Path(__file__).resolve().parent.parent, args.profile, args.variant, command)


if __name__ == "__main__":
    sys.exit(main())
