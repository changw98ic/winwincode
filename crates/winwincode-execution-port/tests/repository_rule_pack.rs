// SPDX-License-Identifier: Apache-2.0

use winwincode_execution_port::repository_rule_pack::{
    PostActionHook, PostActionOutcome, RepositoryRuleEvent, RepositoryRuleFact, RepositoryRulePack,
    RepositoryRulePackError,
};

#[test]
fn repository_rules_lint_explain_and_replay_deterministically() {
    let json = br#"{
  "schemaVersion": 1,
  "rules": [
    {
      "id": "rust.source.verify",
      "version": 1,
      "event": "file_changed",
      "languages": ["rust"],
      "filePatterns": ["crates/*.rs"],
      "outcome": "succeeded",
      "actions": ["require_verification"],
      "priority": 800
    },
    {
      "id": "all.source.block",
      "version": 2,
      "event": "file_changed",
      "languages": [],
      "filePatterns": ["crates/*"],
      "outcome": "succeeded",
      "actions": ["create_machine_blocker"],
      "priority": 900
    }
  ]
}"#;
    let pack = RepositoryRulePack::from_json(json)
        .expect("repository-local JSON should lint")
        .with_project_defaults()
        .expect("project defaults should compose");
    let fact = RepositoryRuleFact {
        event: RepositoryRuleEvent::FileChanged,
        language: Some("rust"),
        path: Some("crates/kernel/src/lib.rs"),
        outcome: Some(PostActionOutcome::Succeeded),
    };
    let first = pack.dry_run(&fact).expect("dry run");
    let second = pack.dry_run(&fact).expect("replay");

    assert_eq!(first, second);
    assert_eq!(
        first
            .matched_rules
            .iter()
            .map(|matched| matched.rule_id.as_str())
            .collect::<Vec<_>>(),
        [
            "all.source.block",
            "rust.source.verify",
            "default.file-change-verification"
        ]
    );
    assert_eq!(
        first.actions,
        [
            PostActionHook::RequireVerification,
            PostActionHook::CreateMachineBlocker
        ]
    );

    let duplicate = br#"{"schemaVersion":1,"rules":[{"id":"same","version":1,"event":"candidate_ready","languages":[],"filePatterns":[],"outcome":null,"actions":["require_verification"],"priority":1},{"id":"same","version":2,"event":"candidate_ready","languages":[],"filePatterns":[],"outcome":null,"actions":["require_verification"],"priority":2}]}"#;
    assert_eq!(
        RepositoryRulePack::from_json(duplicate).expect_err("duplicate rule IDs must fail lint"),
        RepositoryRulePackError::DuplicateRule
    );
}
