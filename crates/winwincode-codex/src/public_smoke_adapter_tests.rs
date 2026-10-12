// SPDX-License-Identifier: Apache-2.0
use super::*;
use winwincode_kernel::{ToolInputContext, ToolInputGatePayload};
#[test]
fn proof_requires_the_private_receipt_and_actual_frozen_source_and_ignores_attempt_timing() {
    let root = std::env::temp_dir().join(format!("wwc-smoke-proof-{}", uuid::Uuid::now_v7()));
    let workspace = root.join("workspace");
    let evidence = root.join("evidence");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir(&evidence).unwrap();
    fs::write(workspace.join("main.py"), "print(1)").unwrap();
    let policy = root.join("policy");
    fs::write(&policy, b"trusted policy").unwrap();
    let adapter = PublicSmokeAdapter {
        workspace: workspace.clone(),
        task_id: "task".into(),
        image: format!("sha256:{}", "a".repeat(64)),
        evidence: evidence.clone(),
        suffixes: vec![".py".into()],
        entry: "main.py".into(),
        configuration_digest: hash(b"config"),
        frozen_files: BTreeMap::from([(policy.clone(), hash(b"trusted policy"))]),
    };
    let bound = adapter
        .snapshot("policy", "offline-worker", "session", "validity")
        .unwrap();
    let context = ToolInputContext {
        request: winwincode_kernel::ToolInputGateRequest {
            thread_id: "thread".into(),
            turn_id: "turn".into(),
            call_id: "call".into(),
            namespace: None,
            tool_name: "public_smoke".into(),
            payload: ToolInputGatePayload::Function {
                arguments: "{}".into(),
            },
        },
        request_sequence: 1,
        operation_digest: "b".repeat(64),
        mcp_server: Some("smoke".into()),
    };
    let mut proofs = Vec::new();
    for (number, elapsed) in [(1, 1.0), (2, 99.0)] {
        let attempt = format!("{number:032x}");
        let directory = evidence.join(&attempt);
        fs::create_dir_all(directory.join("source")).unwrap();
        fs::write(directory.join("source/main.py"), "print(1)").unwrap();
        fs::write(directory.join("stdout.bin"), b"1\n").unwrap();
        fs::write(directory.join("stderr.bin"), b"").unwrap();
        let report = json!({"attemptId":attempt,"status":"evaluated","taskId":"task","taskRevision":REVISION,"imageId":adapter.image,"platform":"linux/arm64","formalBenchmark":false,"gradeScope":"public_examples","source":{"sha256":bound.dependency_digest},"returncode":0,"reason":"finished","publicResult":{"passed":1,"total":1},"elapsedSeconds":elapsed});
        let request = ToolInputProofRequest {
            context: context.clone(),
            snapshot: bound.clone(),
            output: json!({"structuredContent":report}),
        };
        assert!(adapter.verify(&request).is_none());
        fs::write(
            directory.join("result.json"),
            serde_json::to_vec(&request.output["structuredContent"]).unwrap(),
        )
        .unwrap();
        proofs.push(adapter.verify(&request).unwrap());
        fs::write(directory.join("source/main.py"), "print(2)").unwrap();
        assert!(adapter.verify(&request).is_none());
    }
    assert_eq!(proofs[0], proofs[1]);
    fs::write(policy, b"changed policy").unwrap();
    assert!(
        adapter
            .snapshot("policy", "offline-worker", "session", "validity")
            .is_none()
    );
    fs::remove_dir_all(root).unwrap();
}
