import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')

const backupModulePath = join(root, 'crates', 'winwincode-cli', 'src', 'backup.rs')
const cliPath = join(root, 'crates', 'winwincode-cli', 'src', 'cli.rs')
const cliLibPath = join(root, 'crates', 'winwincode-cli', 'src', 'lib.rs')
const storageLibPath = join(root, 'crates', 'winwincode-storage', 'src', 'lib.rs')
const deviceStorePath = join(root, 'crates', 'winwincode-device-client', 'src', 'store.rs')

function read(path) {
  return readFileSync(path, 'utf8')
}

test('backup lane owns one module with a frozen CLI seam', () => {
  const backupModule = read(backupModulePath)
  assert.match(backupModule, /pub fn run_backup\(arguments: &\[String\]\) -> WwcCliExit/)
  assert.match(backupModule, /pub const BACKUP_HELP_LINES: \[&str; \d+\]/)
  assert.match(backupModule, /pub enum BackupStoreKind/)
  assert.match(backupModule, /pub struct BackupManifest/)
  assert.match(backupModule, /pub struct BackupDatabaseEntry/)
  assert.match(backupModule, /pub struct RepairLogEntry/)

  const cliLib = read(cliLibPath)
  assert.match(cliLib, /mod backup;/)
  assert.match(cliLib, /pub use backup::\{BACKUP_HELP_LINES, BackupOutcome, run_backup\};/)

  const cli = read(cliPath)
  assert.match(cli, /"backup" => Ok\(crate::backup::run_backup\(&arguments\[1\.\.\]\)\),/)
  assert.match(cli, /lines\.extend_from_slice\(&crate::backup::BACKUP_HELP_LINES\);/)
})

test('snapshots are consistent cuts from VACUUM INTO, never file-level cold copies', () => {
  const backupModule = read(backupModulePath)
  // The snapshot statement itself.
  assert.match(backupModule, /"VACUUM INTO \?1"/)
  // The intermediate target is renamed into place only after verification.
  assert.match(backupModule, /const VACUUM_TMP_SUFFIX: &str = "\.vacuum-tmp";/)
  assert.match(backupModule, /fs::rename\(vacuum_tmp, snapshot_path\)/)
  // Snapshot creation refuses a damaged source instead of sealing it.
  assert.match(backupModule, /backup\.unhealthy-source/)
  assert.match(backupModule, /PRAGMA quick_check/)
  // Cold copies appear only on the restore side, where the source is a
  // sealed snapshot file, never a live database.
  assert.match(backupModule, /fn place_snapshot\(/)
  const snapshotFunction = backupModule.slice(
    backupModule.indexOf('fn create_snapshot('),
    backupModule.indexOf('fn redact_device_credentials('),
  )
  assert.doesNotMatch(snapshotFunction, /fs::copy/)
})

test('server backup covers the sidecar layout dynamically and gates the product database', () => {
  const backupModule = read(backupModulePath)
  assert.match(backupModule, /const CONTROL_PLANE_DATABASE: &str = "control-plane";/)
  assert.match(backupModule, /const DEVICE_CLIENT_DATABASE: &str = "device-client";/)
  // Server discovery walks every *.sqlite3 sidecar in the data directory.
  assert.match(backupModule, /strip_suffix\("\.sqlite3"\)/)
  // The product database restores only versions the storage adapter accepts.
  assert.match(
    backupModule,
    /const SERVER_CONTROL_PLANE_SCHEMA_VERSION: i64 = 6;/,
  )
  assert.match(
    backupModule,
    /const SERVER_CONTROL_PLANE_MIGRATABLE_VERSIONS: \[i64; 5\] = \[1, 2, 3, 4, 5\];/,
  )
  // The device gate reuses the canonical client-store constant.
  assert.match(
    backupModule,
    /winwincode_device_client::CLIENT_STORE_SCHEMA_VERSION/,
  )
})

test('schema version constants stay locked to the canonical storage sources', () => {
  const backupModule = read(backupModulePath)
  const storageLib = read(storageLibPath)
  const deviceStore = read(deviceStorePath)

  const declaredServer = backupModule.match(
    /const SERVER_CONTROL_PLANE_SCHEMA_VERSION: i64 = (\d+);/,
  )
  assert.ok(declaredServer, 'server version constant declared')
  const storageVersion = storageLib.match(/const SCHEMA_VERSION: i64 = (\d+);/)
  assert.ok(storageVersion, 'storage SCHEMA_VERSION declared')
  assert.equal(
    declaredServer[1],
    storageVersion[1],
    'backup server version must match winwincode-storage SCHEMA_VERSION',
  )

  const storageSupported = storageLib.match(
    /if !matches!\(version, (0 \| [0-9 | ]+) \| SCHEMA_VERSION\)/,
  )
  assert.ok(storageSupported, 'storage supported migration set found')
  for (const version of storageSupported[1].split('|').map((value) => value.trim())) {
    if (version === '0') {
      // Version 0 means "create fresh" at startup. Restore deliberately
      // refuses it: restoring an empty product database would silently wipe
      // durable state, so only migrated schema versions restore.
      assert.doesNotMatch(
        backupModule,
        /MIGRATABLE_VERSIONS: \[i64; \d+\] = \[0,/,
        'restore must not accept an empty v0 product database',
      )
      continue
    }
    assert.match(
      backupModule,
      new RegExp(`const SERVER_CONTROL_PLANE_MIGRATABLE_VERSIONS: \\[i64; \\d+\\] = \\[.*\\b${version}\\b`),
      `storage-migratable version ${version} stays accepted by restore`,
    )
  }

  const declaredDevice = backupModule.match(/CLIENT_STORE_SCHEMA_VERSION/)
  assert.ok(declaredDevice, 'device gate uses the canonical constant')
  const deviceVersion = deviceStore.match(/CLIENT_STORE_SCHEMA_VERSION: i64 = (\d+);/)
  assert.ok(deviceVersion, 'device store schema version declared')
  assert.equal(deviceVersion[1], '6', 'device store stays at schema v6 (candidate registry wave)')
})

test('restore fails closed on unsupported schema versions before touching targets', () => {
  const backupModule = read(backupModulePath)
  assert.match(backupModule, /restore\.unsupported-schema-version/)
  assert.match(backupModule, /restore\.store-mismatch/)
  // Verification runs before any placement and re-reads user_version.
  assert.match(backupModule, /fn verify_backup_directory\(/)
  assert.match(backupModule, /PRAGMA user_version/)
  // Placement is an atomic rename after the credential re-bind, with the
  // replaced database's stale WAL sidecars removed first.
  assert.match(backupModule, /const RESTORE_TMP_SUFFIX: &str = "\.restore-tmp";/)
  assert.match(backupModule, /for suffix in \["-wal", "-shm"\]/)
  assert.match(backupModule, /fs::rename\(&restore_tmp, &target\)/)
})

test('backup artifacts carry no plaintext credential material', () => {
  const backupModule = read(backupModulePath)
  // The device credential column is zeroed in the snapshot.
  assert.match(backupModule, /UPDATE device_credential SET credential_secret = zeroblob\(32\)/)
  assert.match(backupModule, /backup\.secret-detected/)
  // Verify re-checks the redaction contract on every verify run.
  assert.match(backupModule, /fn credential_redaction_finding\(/)
  assert.match(backupModule, /快照携带明文设备凭据/)
  // Marker scan aligned with the evidence-export secret scanner.
  assert.match(backupModule, /wwc_session=/)
  assert.match(backupModule, /authorization:bearer/)
  // Restore re-binds the live credential by digest; cross-device restore is
  // refused because the backup carries no credential material.
  assert.match(backupModule, /fn rebind_device_credentials\(/)
  assert.match(backupModule, /restore\.credential-mismatch/)
  assert.match(backupModule, /restore\.device-credential-unavailable/)
  // Snapshot files land owner-only, never world readable.
  assert.match(backupModule, /fn set_owner_only_permissions\(/)
  assert.match(backupModule, /fn set_directory_permissions\(/)
})

test('repair stays read-only by default, applies bounded actions only, and leaves a trace', () => {
  const backupModule = read(backupModulePath)
  assert.match(backupModule, /const REPAIR_LOG_FILE: &str = "backup-repair-log\.jsonl";/)
  assert.match(backupModule, /fn diagnose_database\(/)
  assert.match(backupModule, /PRAGMA integrity_check/)
  // Only the allowlisted bounded actions exist.
  assert.match(backupModule, /"wal-checkpoint"/)
  assert.match(backupModule, /"stale-temp-cleanup"/)
  // Damage beyond the bounds is never rewritten; the answer is restore.
  assert.match(backupModule, /有界修复不改写数据，请从备份恢复/)
})

test('acceptance behaviors are pinned by named Rust tests inside the module', () => {
  const backupModule = read(backupModulePath)
  const requiredTests = [
    'snapshot_and_restore_round_trip_server_ids_cursors_and_receipts',
    'restore_fails_closed_on_newer_control_plane_schema_version',
    'restore_fails_closed_on_unsupported_device_schema_version',
    'device_snapshot_redacts_credential_and_restore_keeps_identity_state',
    'restore_rejects_snapshot_when_live_device_credential_rotated',
    'restore_rejects_device_backup_without_live_store',
    'verify_rejects_snapshot_carrying_plaintext_device_credential',
    'verify_rejects_tampered_snapshot_bytes',
    'repair_without_apply_is_read_only_and_records_a_trace',
    'repair_apply_checkpoints_wal_and_removes_stale_temp_files',
    'repair_reports_corruption_and_points_to_restore',
  ]
  for (const name of requiredTests) {
    assert.match(backupModule, new RegExp(`fn ${name}\\(`), `missing Rust test: ${name}`)
  }
})

test('help and JSON surface stay scriptable', () => {
  const backupModule = read(backupModulePath)
  for (const line of [
    'wwc backup snapshot --store server|device --data-dir PATH --output PATH [--json]',
    'wwc backup verify --from BACKUP-DIR [--json]',
    'wwc backup restore --store server|device --data-dir PATH --from BACKUP-DIR [--json]',
    'wwc backup repair --store server|device --data-dir PATH [--apply] [--json]',
  ]) {
    assert.ok(backupModule.includes(line), `missing help line: ${line}`)
  }
  assert.match(backupModule, /winwincode\.local-backup\.v1/)
  assert.match(backupModule, /EXIT_ACTION_REQUIRED: i32 = 3;/)
  assert.match(backupModule, /EXIT_DIAGNOSTIC_FAILED: i32 = 1;/)
})
