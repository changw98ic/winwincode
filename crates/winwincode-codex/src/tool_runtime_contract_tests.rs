// SPDX-License-Identifier: Apache-2.0
use super::*;
use crate::adapter::tests::{delegated_record_and_binding, diagnostic_adapter_config};

#[test]
fn resume_requires_the_original_runtime_catalog_and_policy() {
    let root = std::env::temp_dir().join(format!("wwc-runtime-contract-{}", std::process::id()));
    let mut config = diagnostic_adapter_config(&root);
    let (record, _) = delegated_record_and_binding();
    let original = ToolRuntimeContract::capture(&config, &record.agent_config).unwrap();
    let persisted: ToolRuntimeContract =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
    original.validate_resume(Some(&persisted)).unwrap();
    assert!(original.validate_resume(None).is_err());
    config.registered_capabilities.capability_digest =
        Sha256Digest(format!("sha256:{}", "b".repeat(64)));
    let current = ToolRuntimeContract::capture(&config, &record.agent_config).unwrap();
    assert!(current.validate_resume(Some(&persisted)).is_err());
    let mut changed = persisted.clone();
    changed.policy_digest = Sha256Digest(format!("sha256:{}", "c".repeat(64)));
    assert!(original.validate_resume(Some(&changed)).is_err());
    changed = persisted.clone();
    changed.runtime_digest = Sha256Digest(format!("sha256:{}", "d".repeat(64)));
    assert!(original.validate_resume(Some(&changed)).is_err());
    changed = persisted;
    changed.host_binary_digest = Sha256Digest(format!("sha256:{}", "e".repeat(64)));
    assert!(original.validate_resume(Some(&changed)).is_err());
    std::fs::remove_dir_all(root).unwrap();
}
