// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use winwincode_control_plane::knowledge::{
    KnowledgeAccess, KnowledgeAction, KnowledgeCatalogService, KnowledgeCommand, KnowledgeDraft,
    KnowledgeErrorKind, KnowledgeInheritance, KnowledgeLookup, KnowledgeOrigin, KnowledgeScope,
    KnowledgeSource, KnowledgeSourceLocator, KnowledgeStatus, KnowledgeUnavailableReason,
};
use winwincode_domain::{RepositoryId, RequestId, Sha256Digest, UserId};
use winwincode_storage::{ProductStateStorage, SqliteStorage};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}

fn directory(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "winwincode-knowledge-{name}-{}-{}",
        std::process::id(),
        NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ))
}

fn digest(character: char) -> Sha256Digest {
    Sha256Digest(format!("sha256:{}", character.to_string().repeat(64)))
}

fn user() -> UserId {
    UserId(id("usr", 1))
}

fn repository() -> RepositoryId {
    RepositoryId(id("rep", 1))
}

fn access(repositories: Vec<RepositoryId>) -> KnowledgeAccess {
    KnowledgeAccess {
        user_id: user(),
        authorized_repository_ids: repositories,
    }
}

fn draft(
    title: &str,
    body: &str,
    rule_key: &str,
    scope: KnowledgeScope,
    origin: KnowledgeOrigin,
    source_id: &str,
    source_repository: Option<RepositoryId>,
) -> KnowledgeDraft {
    KnowledgeDraft {
        title: title.to_owned(),
        body: body.to_owned(),
        rule_key: rule_key.to_owned(),
        scope,
        origin,
        source: KnowledgeSource {
            source_id: source_id.to_owned(),
            repository_id: source_repository,
            version_digest: digest('a'),
            locator: KnowledgeSourceLocator::Document {
                relative_path: "docs/spec.md".to_owned(),
                start_line: 10,
                end_line: 12,
            },
        },
        expires_at_millis: None,
    }
}

fn command(seed: u64, revision: u64, action: KnowledgeAction) -> KnowledgeCommand {
    KnowledgeCommand {
        actor_user_id: user(),
        request_id: RequestId(id("req", seed)),
        expected_catalog_revision: revision,
        occurred_at_millis: 1_800_000_000_000 + seed,
        action,
    }
}

#[test]
fn only_confirmed_current_knowledge_enters_context() {
    let path = directory("confirmation");
    let mut storage = SqliteStorage::open(&path).expect("open storage");
    let source_id = id("src", 1);
    let entry_id = id("knw", 1);
    {
        let mut service = KnowledgeCatalogService::new(&mut storage);
        service
            .apply(&command(
                1,
                0,
                KnowledgeAction::Create {
                    entry_id: entry_id.clone(),
                    draft: draft(
                        "Run tests",
                        "Run the repository verification command.",
                        "verification.command",
                        KnowledgeScope::Personal,
                        KnowledgeOrigin::MachineSuggestion,
                        &source_id,
                        None,
                    ),
                },
            ))
            .expect("create suggestion");
        assert!(
            service
                .select_context(
                    &access(Vec::new()),
                    None,
                    &BTreeSet::new(),
                    1_800_000_000_100
                )
                .expect("select suggestions")
                .entries
                .is_empty()
        );

        service
            .apply(&command(
                2,
                1,
                KnowledgeAction::Confirm {
                    entry_id: entry_id.clone(),
                },
            ))
            .expect("confirm entry");
        let selected = service
            .select_context(
                &access(Vec::new()),
                None,
                &BTreeSet::new(),
                1_800_000_000_100,
            )
            .expect("select confirmed");
        assert_eq!(selected.entries.len(), 1);
        assert_eq!(
            selected.entries[0].inheritance,
            KnowledgeInheritance::Direct
        );

        let authority = BTreeSet::from(["verification.command".to_owned()]);
        assert!(
            service
                .select_context(&access(Vec::new()), None, &authority, 1_800_000_000_100)
                .expect("current authority wins")
                .entries
                .is_empty()
        );
        service
            .apply(&command(
                3,
                2,
                KnowledgeAction::Edit {
                    entry_id,
                    draft: draft(
                        "Run verification",
                        "Run the current verification command.",
                        "verification.command",
                        KnowledgeScope::Personal,
                        KnowledgeOrigin::UserCorrection,
                        &source_id,
                        None,
                    ),
                },
            ))
            .expect("edit entry");
        assert!(
            service
                .select_context(
                    &access(Vec::new()),
                    None,
                    &BTreeSet::new(),
                    1_800_000_000_100
                )
                .expect("edited entry needs confirmation")
                .entries
                .is_empty()
        );
    }
    drop(storage);
    fs::remove_dir_all(path).expect("remove storage");
}

#[test]
fn repository_acl_filters_before_search_counts_and_source_changes_reconfirm() {
    let path = directory("acl");
    let mut storage = SqliteStorage::open(&path).expect("open storage");
    let repository = repository();
    let entry_id = id("knw", 2);
    let source_id = id("src", 2);
    {
        let mut service = KnowledgeCatalogService::new(&mut storage);
        service
            .apply(&command(
                1,
                0,
                KnowledgeAction::Create {
                    entry_id: entry_id.clone(),
                    draft: draft(
                        "Private repository rule",
                        "Never reveal this private repository content.",
                        "repository.private",
                        KnowledgeScope::Repository {
                            repository_id: repository.clone(),
                        },
                        KnowledgeOrigin::UserAuthored,
                        &source_id,
                        Some(repository.clone()),
                    ),
                },
            ))
            .expect("create repository entry");
        service
            .apply(&command(
                2,
                1,
                KnowledgeAction::Confirm {
                    entry_id: entry_id.clone(),
                },
            ))
            .expect("confirm entry");

        let hidden = service
            .search(&access(Vec::new()), "private")
            .expect("unauthorized search");
        assert_eq!(hidden.matched_count, 0);
        assert!(hidden.entries.is_empty());
        assert_eq!(
            service
                .lookup(&access(Vec::new()), &entry_id)
                .expect("lookup without source access"),
            KnowledgeLookup::Unavailable(KnowledgeUnavailableReason::SourceAccessLost)
        );
        let visible = service
            .search(&access(vec![repository.clone()]), "private")
            .expect("authorized search");
        assert_eq!(visible.matched_count, 1);

        service
            .apply(&command(
                3,
                2,
                KnowledgeAction::SourceVersionChanged {
                    source_id,
                    version_digest: digest('b'),
                },
            ))
            .expect("record changed source");
        let KnowledgeLookup::Found(stale) = service
            .lookup(&access(vec![repository.clone()]), &entry_id)
            .expect("lookup stale source")
        else {
            panic!("stale entry must remain visible for reconfirmation");
        };
        assert_eq!(stale.status, KnowledgeStatus::NeedsReconfirmation);
        assert_eq!(stale.source.version_digest, digest('b'));
        assert!(
            service
                .select_context(
                    &access(vec![repository.clone()]),
                    Some(&repository),
                    &BTreeSet::new(),
                    1_800_000_000_100,
                )
                .expect("stale source stays out of context")
                .entries
                .is_empty()
        );
    }
    drop(storage);
    fs::remove_dir_all(path).expect("remove storage");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the deletion check covers the transaction, raw payload, reopen, and resurrection guard"
)]
fn source_deletion_redacts_current_state_and_survives_reopen() {
    let path = directory("delete");
    let source_id = id("src", 3);
    let entry_id = id("knw", 3);
    let mut storage = SqliteStorage::open(&path).expect("open storage");
    {
        let mut sensitive_draft = draft(
            "Sensitive customer title",
            "Sensitive customer body",
            "customer.secret",
            KnowledgeScope::Personal,
            KnowledgeOrigin::UserCorrection,
            &source_id,
            None,
        );
        sensitive_draft.source.locator = KnowledgeSourceLocator::Document {
            relative_path: "secret/customer-plan.md".to_owned(),
            start_line: 1,
            end_line: 2,
        };
        let mut service = KnowledgeCatalogService::new(&mut storage);
        service
            .apply(&command(
                1,
                0,
                KnowledgeAction::Create {
                    entry_id: entry_id.clone(),
                    draft: sensitive_draft,
                },
            ))
            .expect("create sensitive entry");
        service
            .apply(&command(
                2,
                1,
                KnowledgeAction::DeleteSource {
                    source_id: source_id.clone(),
                },
            ))
            .expect("delete source");
        assert_eq!(
            service
                .lookup(&access(Vec::new()), &entry_id)
                .expect("lookup tombstone"),
            KnowledgeLookup::Unavailable(KnowledgeUnavailableReason::SourceDeleted)
        );
        assert_eq!(
            service
                .search(&access(Vec::new()), "sensitive")
                .expect("search after delete")
                .matched_count,
            0
        );
    }
    let stream = storage
        .last_state_stream_id("knowledge-catalog:")
        .expect("scan state")
        .expect("knowledge stream");
    let payload = String::from_utf8(
        storage
            .load_state(&stream)
            .expect("load state")
            .expect("stored catalog")
            .payload,
    )
    .expect("UTF-8 state");
    for deleted in [
        "Sensitive customer title",
        "Sensitive customer body",
        "secret/customer-plan.md",
        "customer.secret",
    ] {
        assert!(
            !payload.contains(deleted),
            "deleted value remained: {deleted}"
        );
    }
    drop(storage);

    let mut reopened = SqliteStorage::open(&path).expect("reopen storage");
    {
        let mut service = KnowledgeCatalogService::new(&mut reopened);
        assert_eq!(
            service
                .lookup(&access(Vec::new()), &entry_id)
                .expect("lookup after reopen"),
            KnowledgeLookup::Unavailable(KnowledgeUnavailableReason::SourceDeleted)
        );
        let error = service
            .apply(&command(
                3,
                2,
                KnowledgeAction::Create {
                    entry_id: id("knw", 4),
                    draft: draft(
                        "Resurrected",
                        "This must remain blocked.",
                        "customer.secret",
                        KnowledgeScope::Personal,
                        KnowledgeOrigin::MachineSuggestion,
                        &source_id,
                        None,
                    ),
                },
            ))
            .expect_err("tombstoned source must not return");
        assert_eq!(error.kind(), KnowledgeErrorKind::SourceDeleted);
    }
    drop(reopened);
    fs::remove_dir_all(path).expect("remove storage");
}
