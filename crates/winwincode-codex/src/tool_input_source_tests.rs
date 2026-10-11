// SPDX-License-Identifier: Apache-2.0
use super::*;
#[test]
fn source_identity_matches_the_frozen_python_public_smoke_contract() {
    let directory =
        std::env::temp_dir().join(format!("wwc-input-contract-{}", uuid::Uuid::now_v7()));
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("main.py"), "print('trusted snapshot')\n").unwrap();
    fs::write(directory.join("节点😀.py"), "print(2)\n").unwrap();
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/benchmark-public-smoke.py");
    let output = std::process::Command::new("/usr/bin/python3").args(["-I", "-c", "import importlib.util,json,sys; s=importlib.util.spec_from_file_location('smoke',sys.argv[1]);m=importlib.util.module_from_spec(s);s.loader.exec_module(m);print(json.dumps(m.source_identity(sys.argv[2],{'entry':'main.py','suffixes':['.py']})))"])
        .arg(script).arg(&directory).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        python["sha256"],
        source_digest(&directory, &[".py".into()]).unwrap()
    );
    fs::remove_dir_all(directory).unwrap();
}
