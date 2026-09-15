// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};
use winwincode_api::generated::{
    DeviceConfigurationEnvelope, DeviceExtensionMcpConnectionStatus, DeviceExtensionOutcome,
    DeviceProviderOutcome,
};
use winwincode_provider::DeviceProviderStore;

fn encrypt(store: &DeviceProviderStore, mutation: &Value) -> DeviceConfigurationEnvelope {
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let module = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/client/src/device-provider-encryption.ts");
    let mut child = Command::new("node")
        .args([
            "--input-type=module",
            "-e",
            r"
        import { pathToFileURL } from 'node:url';
        import { readFileSync } from 'node:fs';
        const { encryptDeviceExtension } = await import(pathToFileURL(process.argv[1]));
        const {snapshot, mutation, id} = JSON.parse(readFileSync(0, 'utf8'));
        process.stdout.write(JSON.stringify(await encryptDeviceExtension(snapshot, id, mutation)));
    ",
        ])
        .arg(module)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("WebCrypto");
    child.stdin.take().expect("stdin").write_all(&serde_json::to_vec(&json!({
        "snapshot":store.extension_snapshot("device").expect("snapshot"), "mutation":mutation,
        "id":format!("extension_test_{}", SEQUENCE.fetch_add(1,Ordering::Relaxed)),
    })).expect("JSON")).expect("write input");
    let result = child.wait_with_output().expect("encrypt");
    assert!(result.status.success());
    serde_json::from_slice(&result.stdout).expect("envelope")
}

fn apply(store: &mut DeviceProviderStore, mutation: &Value) -> DeviceExtensionOutcome {
    let envelope = encrypt(store, mutation);
    store
        .apply_extension("device", &envelope)
        .expect("apply")
        .outcome
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/extensions/mcp_server.py")
}

#[test]
#[allow(clippy::too_many_lines)]
fn extension_crypto_migration_discovery_refresh_and_failure() {
    let root = std::env::temp_dir().join(format!("wwc-extensions-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let directory = root.join("private");
    let store = DeviceProviderStore::open(&directory).expect("store");
    let key = store
        .snapshot("device")
        .expect("original key")
        .encryption_public_key;
    drop(store);
    // An actual v1 database has only these Provider tables; migration must preserve identity/data.
    let db = rusqlite::Connection::open(directory.join("providers.sqlite3")).expect("v1 fixture");
    db.execute_batch("DROP TABLE extensions; DROP TABLE extension_state; DROP TABLE extension_receipts; PRAGMA user_version=1;").expect("v1 schema");
    let config = json!({"providerId":"retained","displayName":"Retained","endpoint":"https://example.com/v1/messages","protocol":"anthropic_messages","modelIds":["model"],"enabled":true});
    db.execute(
        "INSERT INTO providers VALUES (?1,?2,?3)",
        rusqlite::params![
            "retained",
            config.to_string(),
            b"existing-secret".as_slice()
        ],
    )
    .expect("v1 Provider");
    drop(db);
    let mut store = DeviceProviderStore::open(&directory).expect("migrate v1");
    assert_eq!(
        store
            .snapshot("device")
            .expect("preserved key")
            .encryption_public_key,
        key
    );
    assert_eq!(
        store
            .resolve("retained")
            .expect("preserved Provider")
            .1
            .expose(),
        b"existing-secret"
    );
    assert!(
        store
            .extension_snapshot("device")
            .expect("empty extensions")
            .skills
            .is_empty()
    );

    let source = root.join("skill");
    fs::create_dir(&source).expect("skill directory");
    fs::write(
        source.join("SKILL.md"),
        "---\nname: useful\ndescription: A real imported skill\n---\nUse the bundled resource.\n",
    )
    .expect("skill");
    fs::write(source.join("resource.txt"), "device-only-skill-data").expect("resource");
    let envelope = encrypt(
        &store,
        &json!({"operation":"save_skill","id":"useful","sourcePath":source,"enabled":true}),
    );
    let receipt = store.apply_extension("device", &envelope).expect("import");
    assert_eq!(receipt.outcome, DeviceExtensionOutcome::Saved);
    assert_eq!(
        store
            .apply_extension("device", &envelope)
            .expect("exact replay"),
        receipt
    );
    assert!(store.apply_extension("another-device", &envelope).is_err());
    assert_eq!(
        store
            .apply("device", &envelope)
            .expect("purpose rejected")
            .outcome,
        DeviceProviderOutcome::InvalidRequest
    );
    let mut changed = envelope.clone();
    changed.ciphertext.push('A');
    assert!(store.apply_extension("device", &changed).is_err());
    let stale = encrypt(
        &store,
        &json!({"operation":"delete","kind":"skill","id":"useful"}),
    );
    assert_eq!(
        apply(
            &mut store,
            &json!({"operation":"set_enabled","kind":"skill","id":"useful","enabled":true})
        ),
        DeviceExtensionOutcome::Saved
    );
    assert_eq!(
        store
            .apply_extension("device", &stale)
            .expect("stale")
            .outcome,
        DeviceExtensionOutcome::RevisionConflict
    );
    std::os::unix::fs::symlink(source.join("resource.txt"), source.join("linked"))
        .expect("symlink");
    assert_eq!(
        apply(
            &mut store,
            &json!({"operation":"save_skill","id":"unsafe","sourcePath":source,"enabled":true})
        ),
        DeviceExtensionOutcome::InvalidRequest
    );
    assert_eq!(
        apply(
            &mut store,
            &json!({"operation":"save_skill","id":"invalid","content":"no frontmatter","enabled":true})
        ),
        DeviceExtensionOutcome::InvalidRequest
    );

    let configuration=json!({"command":"python3","args":["-u",fixture()],"env":{"TEST_SECRET":"device-only-mcp-secret"}}).to_string();
    assert_eq!(
        apply(
            &mut store,
            &json!({"operation":"save_mcp","id":"probe","configuration":configuration,"enabled":true})
        ),
        DeviceExtensionOutcome::Saved
    );
    assert_eq!(
        apply(&mut store, &json!({"operation":"test_mcp","id":"probe"})),
        DeviceExtensionOutcome::Tested
    );
    let public = store
        .extension_snapshot("device")
        .expect("connected metadata");
    assert_eq!(public.mcp_servers[0].tool_names, ["fetch_device_proof"]);
    assert_eq!(
        public.mcp_servers[0].connection_status,
        DeviceExtensionMcpConnectionStatus::Ready
    );
    let projection = serde_json::to_string(&public).expect("public JSON");
    assert!(!projection.contains("device-only"));
    let home = root.join("worker");
    assert_eq!(
        store.restore_extensions(&home).expect("freeze")[0].tools,
        ["fetch_device_proof"]
    );
    assert_eq!(
        fs::read_to_string(home.join("skills/useful/resource.txt")).expect("resource"),
        "device-only-skill-data"
    );
    let config = fs::read(home.join("config.toml")).expect("native config");
    assert!(String::from_utf8_lossy(&config).contains("device-only-mcp-secret"));
    assert_eq!(
        apply(
            &mut store,
            &json!({"operation":"delete","kind":"mcp","id":"probe"})
        ),
        DeviceExtensionOutcome::Deleted
    );
    assert_eq!(
        apply(
            &mut store,
            &json!({"operation":"set_enabled","kind":"skill","id":"useful","enabled":false})
        ),
        DeviceExtensionOutcome::Saved
    );
    // Simulate an interrupted installation leaving an uncommitted Skill directory.
    fs::create_dir_all(home.join("skills/interrupted-install")).expect("orphan directory");
    fs::write(home.join("skills/interrupted-install/SKILL.md"), "stale").expect("orphan file");
    drop(store);
    let mut store = DeviceProviderStore::open(&directory).expect("restart");
    assert_eq!(
        store.restore_extensions(&home).expect("resume snapshot")[0].tools,
        ["fetch_device_proof"]
    );
    assert_eq!(
        fs::read(home.join("config.toml")).expect("same config"),
        config
    );
    assert!(!home.join("skills/interrupted-install").exists());
    // A restart resumes the in-flight task above; the next task on this same
    // Worker must remove disabled Skills and deleted MCP services.
    assert!(
        store
            .refresh_extensions(&home)
            .expect("same Worker refresh")
            .is_empty()
    );
    assert!(!home.join("skills/useful/SKILL.md").exists());
    assert!(
        !fs::read_to_string(home.join("config.toml"))
            .expect("updated config")
            .contains("device-only-mcp-secret")
    );
    assert_eq!(
        apply(
            &mut store,
            &json!({
                "operation":"save_skill", "id":"useful", "enabled":true,
                "content":"---\nname: useful\ndescription: Updated skill\n---\nUse the latest recipe.\n"
            })
        ),
        DeviceExtensionOutcome::Saved
    );
    store.refresh_extensions(&home).expect("updated skill");
    assert!(
        fs::read_to_string(home.join("skills/useful/SKILL.md"))
            .expect("new recipe")
            .contains("latest recipe")
    );
    assert!(!home.join("skills/useful/resource.txt").exists());
    assert_eq!(
        apply(
            &mut store,
            &json!({"operation":"delete","kind":"skill","id":"useful"})
        ),
        DeviceExtensionOutcome::Deleted
    );
    store.refresh_extensions(&home).expect("deleted skill");
    assert!(!home.join("skills/useful").exists());
    assert_eq!(
        apply(
            &mut store,
            &json!({"operation":"save_mcp","id":"broken","configuration":"{\"command\":\"/missing/extension-command\"}","enabled":true})
        ),
        DeviceExtensionOutcome::Saved
    );
    let test = encrypt(&store, &json!({"operation":"test_mcp","id":"broken"}));
    let failure = store
        .apply_extension("device", &test)
        .expect("failed connection");
    assert_eq!(failure.outcome, DeviceExtensionOutcome::ConnectionFailed);
    assert_eq!(
        store
            .apply_extension("device", &test)
            .expect("failure replay"),
        failure
    );
    assert!(
        store
            .restore_extensions(&root.join("failed-worker"))
            .expect("failed MCP excluded")
            .is_empty()
    );
    drop(store);
    fs::remove_dir_all(root).expect("cleanup");
}

#[test]
fn streamable_http_discovers_native_tools() {
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);
    let _server = Server(
        Command::new("python3")
            .arg(fixture())
            .args(["--http", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("HTTP server"),
    );
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let root = std::env::temp_dir().join(format!("wwc-mcp-http-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let mut store = DeviceProviderStore::open(&root).expect("store");
    assert_eq!(
        apply(
            &mut store,
            &json!({"operation":"save_mcp","id":"http","configuration":json!({"url":format!("http://127.0.0.1:{port}/mcp")}).to_string(),"enabled":true})
        ),
        DeviceExtensionOutcome::Saved
    );
    assert_eq!(
        apply(&mut store, &json!({"operation":"test_mcp","id":"http"})),
        DeviceExtensionOutcome::Tested
    );
    assert_eq!(
        store
            .extension_snapshot("device")
            .expect("metadata")
            .mcp_servers[0]
            .tool_names,
        ["fetch_device_proof"]
    );
    drop(store);
    fs::remove_dir_all(root).expect("cleanup");
}
