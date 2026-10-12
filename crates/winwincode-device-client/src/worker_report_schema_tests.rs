// SPDX-License-Identifier: Apache-2.0

use super::{DeviceStore, WorkerProcessRecord};
use rusqlite::{Connection, types::Value};
use std::path::Path;
use winwincode_client_port::domain::WorkerLaunchAckStatus;
use winwincode_client_port::messages::{
    ClientToServerMessage, ClientWorkerLaunchAckPayload, ClientWorkerProcessClosure,
    CommandContext, OccupancyCommandContext,
};

const STAMP: &str = "2030-01-01T00:00:00.000Z";
const GRANT: &str = "wlg_00000000000000000000000001";
const LAUNCH_SESSION: &str = "wsn_00000000000000000000000002";

fn retained_launch_receipt() -> Vec<u8> {
    let ack = ClientToServerMessage::WorkerLaunchAck(Box::new(ClientWorkerLaunchAckPayload {
        occupancy: OccupancyCommandContext {
            command: CommandContext {
                expected_revision: 1,
                idempotency_key: "pending-schema-launch-ack".to_owned(),
            },
            occupancy_lease_id: "ocl_00000000000000000000000001".to_owned(),
            occupancy_fencing_token: 1,
        },
        worker_launch_grant_id: GRANT.to_owned(),
        worker_session_id: LAUNCH_SESSION.to_owned(),
        worker_id: "wrk_00000000000000000000000002".to_owned(),
        worker_instance_id: "wki_00000000000000000000000002".to_owned(),
        status: WorkerLaunchAckStatus::RejectedCapacityExhausted,
        error: None,
        process_closure: Some(ClientWorkerProcessClosure {
            worker_launch_grant_id: GRANT.to_owned(),
            client_instance_id: "cix_00000000000000000000000001".to_owned(),
            reporting_client_instance_id: "cix_00000000000000000000000001".to_owned(),
            occupancy_fencing_token: 1,
            never_started: true,
            process_boot_digest: None,
        }),
    }));
    let value = serde_json::json!({"ack": ack});
    let decoded: ClientToServerMessage = serde_json::from_value(value["ack"].clone()).unwrap();
    assert!(matches!(decoded, ClientToServerMessage::WorkerLaunchAck(_)));
    serde_json::to_vec(&value).unwrap()
}

fn terminal_record(root: &Path) -> WorkerProcessRecord {
    WorkerProcessRecord {
        worker_session_id: "wsn_00000000000000000000000001".to_owned(),
        worker_id: "wrk_00000000000000000000000001".to_owned(),
        worker_instance_id: "wki_00000000000000000000000001".to_owned(),
        pid: 999_999,
        process_start_identity: "synthetic exited process".to_owned(),
        repository_binding_id: "rbn_00000000000000000000000001".to_owned(),
        occupancy_lease_id: "ocl_00000000000000000000000001".to_owned(),
        launch_grant_id: "wlg_00000000000000000000000003".to_owned(),
        data_directory: root.display().to_string(),
        state: "exited".to_owned(),
        exit_code: Some(0),
        last_observed_at: STAMP.to_owned(),
    }
}

fn raw_rows(connection: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut statement = connection.prepare(sql).unwrap();
    let columns = statement.column_count();
    statement
        .query_map([], |row| {
            (0..columns).map(|column| row.get(column)).collect()
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

type RetainedReportRows = (Vec<Vec<Value>>, Vec<Vec<Value>>);

fn source_rows(connection: &Connection) -> RetainedReportRows {
    (
        raw_rows(
            connection,
            "SELECT * FROM worker_process_registry ORDER BY worker_session_id",
        ),
        raw_rows(
            connection,
            "SELECT * FROM worker_launch_receipts ORDER BY launch_grant_id",
        ),
    )
}

fn pending_schema(connection: &Connection) -> Vec<Vec<Value>> {
    raw_rows(
        connection,
        "SELECT type,name,sql FROM sqlite_master WHERE name LIKE 'worker_report_%' OR name LIKE 'worker_launch_report_%' ORDER BY type,name",
    )
}

fn missing_pending_case(fault: &str, sql: &str) -> bool {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "wwc-report-schema-{fault}-{}-{unique}",
        std::process::id()
    ));
    let mut store = DeviceStore::open(&root).unwrap();
    let record = terminal_record(&root);
    let receipt = retained_launch_receipt();
    store.put_worker_process(&record).unwrap();
    store
        .retain_worker_launch_receipt(
            GRANT,
            LAUNCH_SESSION,
            "RejectedCapacityExhausted",
            "pending-schema-launch-ack",
            &receipt,
            STAMP,
        )
        .unwrap();
    assert_eq!(
        store.worker_report_candidates("exit", None).unwrap(),
        vec![record.clone()]
    );
    assert_eq!(
        store.worker_report_candidates("reconcile", None).unwrap(),
        vec![record]
    );
    assert_eq!(
        store.pending_worker_launch_receipts().unwrap(),
        vec![(GRANT.to_owned(), receipt)]
    );
    let database = store.database_path().to_owned();
    store.close().unwrap();
    let connection = Connection::open(&database).unwrap();
    let original = source_rows(&connection);
    connection.execute_batch(sql).unwrap();
    let damaged_schema = pending_schema(&connection);
    let (refused, query_counts) = match DeviceStore::open(&root) {
        Ok(store) => {
            let counts = [
                store.worker_report_candidates("exit", None).unwrap().len(),
                store.pending_worker_launch_receipts().unwrap().len(),
            ];
            store.close().unwrap();
            (false, Some(counts))
        }
        Err(_) => (true, None),
    };
    let raw_unchanged = source_rows(&connection) == original;
    let damaged_schema_unchanged = pending_schema(&connection) == damaged_schema;
    assert!(
        raw_unchanged,
        "retained registry and receipt bytes must remain exact"
    );
    println!(
        "DEVICE_SCHEMA {}",
        serde_json::json!({"fault":fault,"before_pending_counts":[1,1],"open_refused":refused,"after_pending_counts":query_counts,"original_source_rows_unchanged":raw_unchanged,"damaged_schema_unchanged":damaged_schema_unchanged})
    );
    drop(connection);
    std::fs::remove_dir_all(root).unwrap();
    refused && damaged_schema_unchanged
}

#[test]
fn current_schema_rejects_missing_pending_tables() {
    let outcomes = [
        missing_pending_case("worker-report-pending", "DROP TABLE worker_report_pending;"),
        missing_pending_case(
            "worker-launch-report-pending",
            "DROP TABLE worker_launch_report_pending;",
        ),
    ];
    assert!(
        outcomes.into_iter().all(|refused| refused),
        "current schema must reject missing queues without rebuilding them"
    );
}

struct LatePendingGate {
    enter: std::sync::mpsc::Sender<std::time::Instant>,
    locked: std::sync::mpsc::Receiver<()>,
}

thread_local! {
    static LATE_PENDING_GATE: std::cell::RefCell<Option<LatePendingGate>> = const { std::cell::RefCell::new(None) };
    static LAST_PENDING_SEAM: std::cell::Cell<Option<(std::time::Instant, std::time::Instant)>> = const { std::cell::Cell::new(None) };
}

pub(super) fn before_schema_prepare_for_test(open_deadline: std::time::Instant) {
    let gate = LATE_PENDING_GATE.with(|slot| slot.borrow_mut().take());
    if let Some(gate) = gate {
        let near_deadline = open_deadline
            .checked_sub(std::time::Duration::from_millis(200))
            .unwrap();
        if let Some(remaining) = near_deadline.checked_duration_since(std::time::Instant::now()) {
            std::thread::sleep(remaining);
        }
        gate.enter.send(open_deadline).unwrap();
        gate.locked
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        LAST_PENDING_SEAM
            .with(|sample| sample.set(Some((open_deadline, std::time::Instant::now()))));
    }
}

fn late_pending_writer_root() -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "wwc-report-late-writer-{}-{unique}",
        std::process::id()
    ))
}

#[test]
fn schema_validation_uses_the_original_open_deadline_under_a_real_writer_lock() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::{Duration, Instant};
    let root = late_pending_writer_root();
    let mut store = DeviceStore::open(&root).unwrap();
    store.put_worker_process(&terminal_record(&root)).unwrap();
    let receipt = retained_launch_receipt();
    store
        .retain_worker_launch_receipt(
            GRANT,
            LAUNCH_SESSION,
            "RejectedCapacityExhausted",
            "pending-schema-launch-ack",
            &receipt,
            STAMP,
        )
        .unwrap();
    assert_eq!(
        store.worker_report_candidates("exit", None).unwrap().len(),
        1
    );
    assert_eq!(store.pending_worker_launch_receipts().unwrap().len(), 1);
    let database = store.database_path().to_owned();
    store.close().unwrap();
    let mut connection = Connection::open(&database).unwrap();
    let original = source_rows(&connection);
    let (enter, entered) = mpsc::channel::<Instant>();
    let (locked, wait_locked) = mpsc::channel();
    let released = Arc::new(AtomicBool::new(false));
    let released_writer = Arc::clone(&released);
    let writer = std::thread::spawn(move || {
        let original_deadline = entered.recv_timeout(Duration::from_secs(10)).unwrap();
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        locked.send(()).unwrap();
        let release_at = original_deadline
            .checked_add(Duration::from_millis(400))
            .unwrap();
        if let Some(remaining) = release_at.checked_duration_since(Instant::now()) {
            std::thread::sleep(remaining);
        }
        transaction.rollback().unwrap();
        released_writer.store(true, Ordering::Release);
    });
    LATE_PENDING_GATE.with(|slot| {
        *slot.borrow_mut() = Some(LatePendingGate {
            enter,
            locked: wait_locked,
        });
    });
    LAST_PENDING_SEAM.with(|sample| sample.set(None));
    let started = Instant::now();
    let outcome = DeviceStore::open(&root);
    let returned = Instant::now();
    let returned_while_writer_held = !released.load(Ordering::Acquire);
    let (original_deadline, seam_at) = LAST_PENDING_SEAM
        .with(std::cell::Cell::take)
        .expect("schema seam must run after the opening pragmas");
    let remaining_at_seam = original_deadline
        .checked_duration_since(seam_at)
        .unwrap_or_default();
    let refused = outcome.is_err();
    let error = outcome.as_ref().err().map(ToString::to_string);
    if let Ok(store) = outcome {
        store.close().unwrap();
    }
    writer.join().unwrap();
    let connection = Connection::open(&database).unwrap();
    assert_eq!(source_rows(&connection), original);
    println!(
        "DEVICE_SCHEMA {}",
        serde_json::json!({"fault":"late-pending-real-writer","late_seam_executed":true,"remaining_at_seam_ms":remaining_at_seam.as_millis(),"open_duration_ms":returned.duration_since(started).as_millis(),"original_deadline_elapsed_at_return":returned>=original_deadline,"writer_held_across_original_deadline":true,"returned_while_writer_held":returned_while_writer_held,"open_refused":refused,"error":error,"original_source_rows_unchanged":true})
    );
    drop(connection);
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        !remaining_at_seam.is_zero(),
        "schema validation must enter before the original deadline"
    );
    assert!(
        refused && returned_while_writer_held,
        "schema preparation must keep the original open deadline instead of waiting for a new runtime five-second timeout"
    );
}

fn retained_pending_fixture(
    label: &str,
) -> (std::path::PathBuf, std::path::PathBuf, WorkerProcessRecord) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "wwc-report-contract-{label}-{}-{unique}",
        std::process::id()
    ));
    let mut store = DeviceStore::open(&root).unwrap();
    let record = terminal_record(&root);
    store.put_worker_process(&record).unwrap();
    store
        .retain_worker_launch_receipt(
            GRANT,
            LAUNCH_SESSION,
            "RejectedCapacityExhausted",
            "pending-schema-launch-ack",
            &retained_launch_receipt(),
            STAMP,
        )
        .unwrap();
    assert_eq!(
        store.worker_report_candidates("exit", None).unwrap().len(),
        1
    );
    assert_eq!(store.pending_worker_launch_receipts().unwrap().len(), 1);
    let database = store.database_path().to_owned();
    store.close().unwrap();
    (root, database, record)
}

fn guarded_pending_schema(connection: &Connection) -> Vec<Vec<Value>> {
    raw_rows(
        connection,
        "SELECT type,name,sql FROM sqlite_master WHERE name IN ('worker_process_registry_state_lease','worker_process_registry_active','worker_report_pending','worker_report_after_insert','worker_report_after_update','worker_launch_report_pending','worker_launch_report_after_insert') ORDER BY type,name",
    )
}

const DROP_PENDING_LAYOUT: &str =
    "DROP TRIGGER worker_report_after_insert; DROP TRIGGER worker_report_after_update;
    DROP TRIGGER worker_launch_report_after_insert; DROP INDEX worker_process_registry_state_lease;
    DROP INDEX worker_process_registry_active; DROP TABLE worker_report_pending;
    DROP TABLE worker_launch_report_pending;";

#[test]
fn current_schema_rejects_each_weakened_report_object() {
    let faults = [
        (
            "state-lease-index",
            "DROP INDEX worker_process_registry_state_lease; CREATE INDEX worker_process_registry_state_lease ON worker_process_registry(state);",
        ),
        (
            "active-index",
            "DROP INDEX worker_process_registry_active; CREATE INDEX worker_process_registry_active ON worker_process_registry(occupancy_lease_id,worker_session_id);",
        ),
        (
            "report-table",
            "DROP TABLE worker_report_pending; CREATE TABLE worker_report_pending(worker_session_id TEXT,family TEXT);",
        ),
        (
            "report-insert-trigger",
            "DROP TRIGGER worker_report_after_insert; CREATE TRIGGER worker_report_after_insert AFTER INSERT ON worker_process_registry BEGIN SELECT 1; END;",
        ),
        (
            "report-update-trigger",
            "DROP TRIGGER worker_report_after_update; CREATE TRIGGER worker_report_after_update AFTER UPDATE ON worker_process_registry BEGIN SELECT 1; END;",
        ),
        (
            "launch-table",
            "DROP TABLE worker_launch_report_pending; CREATE TABLE worker_launch_report_pending(launch_grant_id TEXT);",
        ),
        (
            "launch-insert-trigger",
            "DROP TRIGGER worker_launch_report_after_insert; CREATE TRIGGER worker_launch_report_after_insert AFTER INSERT ON worker_launch_receipts BEGIN SELECT 1; END;",
        ),
    ];
    let mut outcomes = Vec::new();
    for (fault, sql) in faults {
        let (root, database, _) = retained_pending_fixture(fault);
        let connection = Connection::open(database).unwrap();
        let original = source_rows(&connection);
        connection.execute_batch(sql).unwrap();
        let schema = guarded_pending_schema(&connection);
        let refused = match DeviceStore::open(&root) {
            Err(_) => true,
            Ok(store) => {
                store.close().unwrap();
                false
            }
        };
        let source_unchanged = source_rows(&connection) == original;
        let schema_unchanged = guarded_pending_schema(&connection) == schema;
        println!(
            "DEVICE_SCHEMA {}",
            serde_json::json!({"fault":fault,"open_refused":refused,"original_source_rows_unchanged":source_unchanged,"weakened_schema_not_rebuilt":schema_unchanged})
        );
        outcomes.push(refused && source_unchanged && schema_unchanged);
        drop(connection);
        std::fs::remove_dir_all(root).unwrap();
    }
    assert!(
        outcomes.into_iter().all(|ok| ok),
        "current schema must validate the guarded table, index and trigger definitions"
    );
}

#[test]
fn consumed_reports_stay_consumed_after_reopen() {
    let (root, database, record) = retained_pending_fixture("consumed-reopen");
    let connection = Connection::open(&database).unwrap();
    let original = source_rows(&connection);
    let mut store = DeviceStore::open(&root).unwrap();
    assert_eq!(
        store.worker_report_candidates("exit", None).unwrap(),
        vec![record.clone()]
    );
    assert_eq!(
        store.worker_report_candidates("reconcile", None).unwrap(),
        vec![record.clone()]
    );
    assert_eq!(
        store.pending_worker_launch_receipts().unwrap(),
        vec![(GRANT.to_owned(), retained_launch_receipt())]
    );
    store.worker_report_queued("exit", &record).unwrap();
    store.worker_report_queued("reconcile", &record).unwrap();
    store.worker_launch_report_queued(GRANT).unwrap();
    store.close().unwrap();
    let schema = guarded_pending_schema(&connection);
    let store = DeviceStore::open(&root).unwrap();
    assert!(
        store
            .worker_report_candidates("exit", None)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .worker_report_candidates("reconcile", None)
            .unwrap()
            .is_empty()
    );
    assert!(store.pending_worker_launch_receipts().unwrap().is_empty());
    store.close().unwrap();
    assert_eq!(source_rows(&connection), original);
    assert_eq!(guarded_pending_schema(&connection), schema);
    println!(
        "DEVICE_SCHEMA {}",
        serde_json::json!({"fault":"reports-consumed-reopen","first_pending_counts":[1,1,1],"consumed_reopen_counts":[0,0,0],"original_source_rows_unchanged":true,"schema_unchanged_on_reopen":true})
    );
    drop(connection);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn partial_report_layout_is_refused_without_rebuilding() {
    let (root, database, _) = retained_pending_fixture("late-rollback");
    let connection = Connection::open(&database).unwrap();
    connection.execute_batch(DROP_PENDING_LAYOUT).unwrap();
    connection
        .execute_batch("CREATE TABLE worker_launch_report_pending(launch_grant_id TEXT);")
        .unwrap();
    let original = source_rows(&connection);
    let schema = guarded_pending_schema(&connection);
    let result = DeviceStore::open(&root);
    let refused = match result {
        Err(_) => true,
        Ok(store) => {
            store.close().unwrap();
            false
        }
    };
    assert_eq!(source_rows(&connection), original);
    assert_eq!(guarded_pending_schema(&connection), schema);
    println!(
        "DEVICE_SCHEMA {}",
        serde_json::json!({"fault":"partial-report-layout","open_refused":refused,"original_source_rows_unchanged":true,"all_schema_rolled_back":true})
    );
    drop(connection);
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        refused,
        "a partial report layout must be rejected without rebuilding it"
    );
}

mod trigger_upsert_regression {
    use super::{DeviceStore, STAMP, pending_schema, raw_rows, source_rows, terminal_record};
    use rusqlite::{Connection, OpenFlags};
    use std::path::PathBuf;
    use winwincode_client_port::domain::ClientWorkerRunState;
    use winwincode_client_port::messages::{
        CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientToServerEnvelope, ClientToServerMessage,
        ClientWorkerStatePayload,
    };

    fn fixture_root(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "wwc-report-upsert-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    fn families(store: &DeviceStore, session: &str) -> Vec<String> {
        let mut statement = store
            .connection()
            .unwrap()
            .prepare(
                "SELECT family FROM worker_report_pending WHERE worker_session_id=?1 ORDER BY family",
            )
            .unwrap();
        statement
            .query_map([session], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn retain_terminal_envelope(store: &mut DeviceStore, record: &super::WorkerProcessRecord) {
        let envelope = ClientToServerEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: "msg_REPORTUPSERTERMINAL".to_owned(),
            client_node_id: "dvn_REPORTUPSERT".to_owned(),
            client_instance_id: "cix_000000000000000000000001".to_owned(),
            sequence: 1,
            occurred_at: STAMP.to_owned(),
            message: ClientToServerMessage::WorkerState(ClientWorkerStatePayload {
                occupancy_lease_id: Some(record.occupancy_lease_id.clone()),
                worker_session_id: record.worker_session_id.clone(),
                worker_instance_id: record.worker_instance_id.clone(),
                state: ClientWorkerRunState::Stopped,
                exit_code: Some(0),
                observed_at: STAMP.to_owned(),
                process_closure: None,
            }),
        };
        store
            .append_outbox_envelope(&envelope, "client.worker.state")
            .unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn worker_report_upserts_keep_pending_membership_and_exact_new_instance_marks() {
        let root = fixture_root("membership");
        let mut store = DeviceStore::open(&root).unwrap();
        let mut record = terminal_record(&root);
        record.state = "running".to_owned();
        record.exit_code = None;
        store.put_worker_process(&record).unwrap();
        assert_eq!(families(&store, &record.worker_session_id), ["reconcile"]);
        store.put_worker_process(&record).unwrap();
        assert_eq!(families(&store, &record.worker_session_id), ["reconcile"]);

        record.state = "exited".to_owned();
        record.exit_code = Some(0);
        store.put_worker_process(&record).unwrap();
        retain_terminal_envelope(&mut store, &record);
        let retained_outbox = store.pending_outbox_envelopes().unwrap();
        assert_eq!(
            families(&store, &record.worker_session_id),
            ["exit", "reconcile"]
        );
        for (state, code) in [("crashed", Some(1)), ("missing", None)] {
            record.state = state.to_owned();
            record.exit_code = code;
            store.put_worker_process(&record).unwrap();
            assert_eq!(
                families(&store, &record.worker_session_id),
                ["exit", "reconcile"]
            );
        }
        let old_record = record.clone();
        record.worker_instance_id = "wki_00000000000000000000000002".to_owned();
        record.pid -= 1;
        record.process_start_identity = "synthetic replacement process".to_owned();
        record.state = "running".to_owned();
        store.put_worker_process(&record).unwrap();
        assert_eq!(
            store.worker_process(&record.worker_session_id).unwrap(),
            Some(record.clone())
        );
        assert_eq!(
            families(&store, &record.worker_session_id),
            ["exit", "reconcile"]
        );
        for family in ["exit", "reconcile"] {
            store.worker_report_queued(family, &old_record).unwrap();
        }
        assert_eq!(
            families(&store, &record.worker_session_id),
            ["exit", "reconcile"]
        );
        store.worker_report_queued("reconcile", &record).unwrap();
        assert_eq!(families(&store, &record.worker_session_id), ["exit"]);
        record.state = "exited".to_owned();
        record.exit_code = Some(0);
        store.put_worker_process(&record).unwrap();
        for family in ["exit", "reconcile"] {
            store.worker_report_queued(family, &old_record).unwrap();
        }
        assert_eq!(
            families(&store, &record.worker_session_id),
            ["exit", "reconcile"]
        );
        for family in ["exit", "reconcile"] {
            store.worker_report_queued(family, &record).unwrap();
        }
        assert!(families(&store, &record.worker_session_id).is_empty());

        let mut inserted_terminal = terminal_record(&root);
        inserted_terminal.worker_session_id = "wsn_00000000000000000000000002".to_owned();
        store.put_worker_process(&inserted_terminal).unwrap();
        assert_eq!(
            families(&store, &inserted_terminal.worker_session_id),
            ["exit", "reconcile"]
        );
        for family in ["exit", "reconcile"] {
            store
                .worker_report_queued(family, &inserted_terminal)
                .unwrap();
        }
        assert_eq!(store.pending_outbox_envelopes().unwrap(), retained_outbox);
        store.close().unwrap();
        let reopened = DeviceStore::open(&root).unwrap();
        assert!(families(&reopened, &record.worker_session_id).is_empty());
        assert!(families(&reopened, &inserted_terminal.worker_session_id).is_empty());
        assert_eq!(
            reopened.pending_outbox_envelopes().unwrap(),
            retained_outbox
        );
        println!(
            "DEVICE_TRIGGER {}",
            serde_json::json!({"case":"actual-registry-upsert-membership","insert_running_and_terminal":true,"same_instance_replay":true,"terminal_updates":2,"fresh_instance_respawn":true,"both_pending_families_preserved":true,"old_record_marks_cannot_consume_replacement":true,"exact_current_marks_consumed":true,"retained_outbox_bytes_unchanged":true,"reopen_does_not_requeue_consumed":true})
        );
        reopened.close().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_report_changed_trigger_layout_is_refused_without_rewriting_evidence() {
        let root = fixture_root("old-wip-seal");
        let mut store = DeviceStore::open(&root).unwrap();
        let record = terminal_record(&root);
        store.put_worker_process(&record).unwrap();
        retain_terminal_envelope(&mut store, &record);
        store.close().unwrap();
        let path = root.join("device-client.sqlite3");
        let connection = Connection::open(&path).unwrap();
        // Changing trigger conflict behavior must not rewrite retained evidence.
        connection.execute_batch("DROP TRIGGER worker_report_after_insert;
            DROP TRIGGER worker_report_after_update;
            CREATE TRIGGER worker_report_after_insert AFTER INSERT ON worker_process_registry BEGIN
            INSERT OR IGNORE INTO worker_report_pending VALUES (NEW.worker_session_id,'reconcile');
            INSERT OR IGNORE INTO worker_report_pending SELECT NEW.worker_session_id,'exit' WHERE NEW.state IN ('exited','crashed','missing'); END;
            CREATE TRIGGER worker_report_after_update AFTER UPDATE ON worker_process_registry WHEN OLD.worker_instance_id != NEW.worker_instance_id OR OLD.state != NEW.state OR OLD.exit_code IS NOT NEW.exit_code BEGIN
            INSERT OR IGNORE INTO worker_report_pending VALUES (NEW.worker_session_id,'reconcile');
            INSERT OR IGNORE INTO worker_report_pending SELECT NEW.worker_session_id,'exit' WHERE NEW.state IN ('exited','crashed','missing'); END;").unwrap();
        let source_before = source_rows(&connection);
        let schema_before = pending_schema(&connection);
        let pending_before = raw_rows(
            &connection,
            "SELECT family,worker_session_id FROM worker_report_pending ORDER BY family,worker_session_id",
        );
        let outbox_before = raw_rows(
            &connection,
            "SELECT * FROM client_outbox ORDER BY outbox_sequence",
        );
        drop(connection);
        let refused = DeviceStore::open(&root).is_err();
        let connection =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(source_rows(&connection), source_before);
        assert_eq!(pending_schema(&connection), schema_before);
        assert_eq!(
            raw_rows(
                &connection,
                "SELECT family,worker_session_id FROM worker_report_pending ORDER BY family,worker_session_id"
            ),
            pending_before
        );
        assert_eq!(
            raw_rows(
                &connection,
                "SELECT * FROM client_outbox ORDER BY outbox_sequence"
            ),
            outbox_before
        );
        println!(
            "DEVICE_TRIGGER {}",
            serde_json::json!({"case":"changed-trigger-layout","open_refused":refused,"original_source_rows_unchanged":true,"retained_outbox_bytes_unchanged":true,"pending_membership_unchanged":true,"old_schema_not_rebuilt":true})
        );
        drop(connection);
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            refused,
            "changed layout must remain intact when current schema validation fails"
        );
    }
}
