import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True

MODULE = Path(__file__).resolve().parent.parent / 'scripts/rust-build-cache.py'
spec = importlib.util.spec_from_file_location('cache', MODULE)
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)


class CacheTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='wwc-cache-test-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        tools = self.root / 'tools'
        tools.mkdir()
        for name in ('cargo', 'rustc'):
            file = tools / name
            file.write_text('#!/bin/sh\nprintf "fixture-tool-1\\n"\n')
            file.chmod(0o700)
        self.env = {'PATH': str(tools)}
        self.command = [sys.executable, '-c', 'pass']

    def run_build(self, command=None, profile='debug', env=None):
        return cache.run_cached(self.root, profile, 'all-features', command or self.command, env or self.env)

    def entries(self, profile='debug'):
        return list((self.root / '.cache/rust-build-v1' / profile).iterdir())

    def expire(self, entry, now=200000):
        record = cache.read_record(entry / 'usage.json')
        record['last_used'] = now - cache.MAX_IDLE_SECONDS
        cache.write_record(entry / 'usage.json', record)
        return now

    def child(self, expression):
        setup = f"import sys; sys.dont_write_bytecode=True; import importlib.util; from pathlib import Path; s=importlib.util.spec_from_file_location('cache',{str(MODULE)!r}); c=importlib.util.module_from_spec(s); s.loader.exec_module(c); root=Path({str(self.root)!r}); "
        return subprocess.Popen([sys.executable, '-c', setup + expression], stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def test_exact_24_hour_boundary_uses_metadata_not_mtime(self):
        self.run_build()
        entry = self.entries()[0]
        now = self.expire(entry)
        os.utime(entry, (0, 0))
        cache.cleanup(self.root, now - 0.001)
        self.assertTrue(entry.exists())
        os.utime(entry, (now, now))
        cache.cleanup(self.root, now)
        self.assertFalse(entry.exists())

    def test_hit_refreshes_use_before_expiration_cleanup(self):
        self.run_build()
        entry = self.entries()[0]
        now = self.expire(entry)
        with patch.object(cache.time, 'time', return_value=now + 1):
            self.assertEqual(self.run_build(), 0)
        self.assertTrue(entry.exists())
        self.assertEqual(cache.read_record(entry / 'usage.json')['last_used'], now + 1)

    def test_active_kernel_lock_protects_cache_across_processes(self):
        self.run_build()
        entry = self.entries()[0]
        now = self.expire(entry)
        fd = cache.lock_entry(cache.cache_root(self.root), 'debug', entry.name)
        try:
            child = self.child(f'c.cleanup(root, {now})')
            self.assertEqual(child.wait(timeout=10), 0)
            self.assertTrue(entry.exists())
        finally:
            os.close(fd)
        cache.cleanup(self.root, now)
        self.assertFalse(entry.exists())

    def test_two_cleaners_racing_delete_one_whole_bucket(self):
        self.run_build()
        entry = self.entries()[0]
        now = self.expire(entry)
        children = [self.child(f'c.cleanup(root, {now})') for _ in range(2)]
        for child in children:
            self.assertEqual(child.wait(timeout=10), 0)
        self.assertFalse(entry.exists())

    def test_waiting_build_and_cleanup_do_not_lose_in_use_bucket(self):
        script = self.root / 'build.py'
        script.write_text("import os,time; from pathlib import Path; p=Path(os.environ['CARGO_BUILD_BUILD_DIR']); marker=p/'active'; assert not marker.exists(); marker.touch(); time.sleep(0.15); marker.unlink()")
        command = [sys.executable, str(script)]
        children = [self.child(f'raise SystemExit(c.run_cached(root,"debug","all-features",{command!r},{self.env!r}))') for _ in range(2)]
        for child in children:
            out, err = child.communicate(timeout=10)
            self.assertEqual(child.returncode, 0, err)
        self.assertEqual(len(self.entries()), 1)

    def test_failed_build_still_refreshes_and_cleans_without_masking_exit_code(self):
        self.run_build()
        expired = self.entries()[0]
        self.expire(expired, time.time() - 1)
        self.assertEqual(self.run_build([sys.executable, '-c', 'raise SystemExit(37)']), 37)
        self.assertFalse(expired.exists())
        with patch.object(cache, 'cleanup', side_effect=OSError('fixture cleanup error')):
            self.assertEqual(self.run_build([sys.executable, '-c', 'raise SystemExit(42)']), 42)
        with patch.object(cache, 'write_record', side_effect=[None, OSError('fixture usage error')]):
            self.assertEqual(self.run_build([sys.executable, '-c', 'raise SystemExit(43)']), 43)

    def test_symlink_roots_buckets_metadata_and_foreign_files_are_safe(self):
        self.run_build()
        base = cache.cache_root(self.root)
        outside = self.root / 'source-and-user-data'
        outside.mkdir()
        sentinel = outside / 'keep'
        sentinel.write_text('source')
        (base / 'debug' / ('a' * 64)).symlink_to(outside, target_is_directory=True)
        foreign = base / 'debug' / ('b' * 64)
        foreign.mkdir()
        (foreign / 'usage.json').symlink_to(sentinel)
        cache.cleanup(self.root, time.time() + 3 * cache.MAX_IDLE_SECONDS)
        self.assertEqual(sentinel.read_text(), 'source')
        self.assertTrue(foreign.exists())
        (base / 'release').rmdir()
        (base / 'release').symlink_to(outside, target_is_directory=True)
        with self.assertRaises(ValueError):
            cache.cleanup(self.root)
        self.assertTrue(sentinel.exists())

    def test_compatibility_isolates_profiles_features_flags_targets_and_toolchains(self):
        def key(profile='debug', variant='default', command=None, extra=None):
            return cache.compatibility_key(self.root, profile, variant, command or self.command, {**self.env, **(extra or {})})
        original = key()
        common = dict(root=self.root, profile='debug', variant='default-features', environment=self.env)
        self.assertEqual(cache.compatibility_key(command=['cargo','build','--locked','-p','one','--bin','one'], **common), cache.compatibility_key(command=['cargo','build','-p','two','--bin','two','--locked','--offline'], **common))
        self.assertEqual(key(variant='all-features', command=['node','scripts/test-rust-workspace.mjs','--build-helper']), key(variant='all-features', command=['node','scripts/rust-test-archive.mjs','produce']))
        for changed in [key(profile='release'), key(variant='all-features'), key(extra={'RUSTFLAGS': '-C opt-level=2'}), key(extra={'CARGO_BUILD_TARGET': 'aarch64-apple-darwin'}), key(command=['cargo', 'build', '--features', 'test-support'])]:
            self.assertNotEqual(original, changed)
        (self.root / 'tools/rustc').write_text('#!/bin/sh\nprintf "fixture-tool-2\\n"\n')
        self.assertNotEqual(original, key())

    def test_unowned_and_other_project_buckets_are_never_deleted(self):
        self.run_build()
        entry = self.entries()[0]
        now = self.expire(entry)
        record = cache.read_record(entry / 'usage.json')
        record['project'] = '/another/project'
        cache.write_record(entry / 'usage.json', record)
        cache.cleanup(self.root, now)
        self.assertTrue(entry.exists())
        with self.assertRaises(ValueError):
            self.run_build()


if __name__ == '__main__':
    unittest.main()
