// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_control_plane::PublicationEnterpriseUsageReconciler;
use winwincode_publication::{
    PublicationOperation, PublicationOperationKind, PublicationPort, PublicationPortError,
    PublicationPortMutation, PublicationPortObservation, PublicationResourceFact,
    PublicationResourceKind,
    test_support::{current_policy_coordinator, current_publication_fixture},
};
use winwincode_storage::{
    EnterpriseUsageFilter, EnterpriseUsageMeasure, EnterpriseUsageSource,
    EnterpriseUsageSourceKind, ProductStateStorage, SqliteStorage,
};

#[derive(Default)]
struct AppliedProvider {
    resources: HashMap<String, Option<PublicationResourceFact>>,
}

impl PublicationPort for AppliedProvider {
    fn lookup(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortObservation, PublicationPortError> {
        Ok(self.resources.get(operation.operation_key()).map_or_else(
            || PublicationPortObservation::absent(operation),
            |resource| {
                PublicationPortObservation::found(
                    operation,
                    operation.request_sha256(),
                    resource.clone(),
                )
            },
        ))
    }

    fn apply(
        &mut self,
        operation: &PublicationOperation,
    ) -> Result<PublicationPortMutation, PublicationPortError> {
        let resource = (operation.kind() == PublicationOperationKind::PullRequest).then(|| {
            PublicationResourceFact::try_new(
                PublicationResourceKind::GitHubPullRequest,
                "example/widget",
                42,
            )
            .expect("canonical pull request")
        });
        self.resources
            .insert(operation.operation_key().to_owned(), resource.clone());
        Ok(PublicationPortMutation::applied(operation, resource, true))
    }
}

#[test]
fn publication_sources_reconcile_exactly_once_across_page_restart_and_full_replay() {
    let root = temporary_root();
    let fixture = current_publication_fixture();
    let mut storage = SqliteStorage::open(&root).expect("open storage");
    let mut provider = AppliedProvider::default();
    current_policy_coordinator(&mut storage, &mut provider)
        .publish(
            fixture.publish_context(),
            fixture.publish_command(),
            fixture.authorization(),
        )
        .expect("persist Publication intent");
    current_policy_coordinator(&mut storage, &mut provider)
        .resume(fixture.publication_id(), fixture.resume_time_millis())
        .expect("confirm four provider mutations");

    let first = PublicationEnterpriseUsageReconciler::new(&mut storage)
        .reconcile_publication_page(None, 2)
        .expect("reconcile first bounded page");
    assert_eq!(first.snapshot_sequence, 4);
    assert_eq!((first.source_entries, first.inserted_entries), (2, 2));
    assert_eq!(first.replayed_entries, 0);
    let cursor = first.next.expect("second source page");

    Box::new(storage).close().expect("close after first page");
    let mut restarted = SqliteStorage::open(&root).expect("restart storage");
    let second = PublicationEnterpriseUsageReconciler::new(&mut restarted)
        .reconcile_publication_page(Some(&cursor), 2)
        .expect("resume the fixed source snapshot");
    assert_eq!(second.snapshot_sequence, 4);
    assert_eq!((second.source_entries, second.inserted_entries), (2, 2));
    assert_eq!(second.replayed_entries, 0);
    assert!(second.next.is_none());

    Box::new(restarted)
        .close()
        .expect("close after second page");
    let mut replayed = SqliteStorage::open(&root).expect("restart before exact replay");
    let replay = PublicationEnterpriseUsageReconciler::new(&mut replayed)
        .reconcile_publication_page(None, 200)
        .expect("replay every immutable source receipt");
    assert_eq!(replay.source_entries, 4);
    assert_eq!(replay.inserted_entries, 0);
    assert_eq!(replay.replayed_entries, 4);

    let page = replayed
        .enterprise_usage_ledger()
        .expect("open enterprise Usage ledger")
        .scan(
            &EnterpriseUsageFilter {
                source_kind: Some(EnterpriseUsageSourceKind::Publication),
                ..EnterpriseUsageFilter::default()
            },
            None,
            200,
        )
        .expect("read reconciled Publication Usage");
    assert_eq!(page.entries.len(), 4);
    assert!(page.next.is_none());
    assert!(page.entries.iter().all(|entry| {
        let EnterpriseUsageSource::Publication {
            publication_id,
            operation_key,
            request_sha256,
        } = &entry.fact.source
        else {
            return false;
        };
        publication_id == fixture.publication_id()
            && !operation_key.is_empty()
            && request_sha256.starts_with("sha256:")
            && entry.fact.attribution.organization_id == *fixture.attribution().organization_id()
            && entry.fact.attribution.workspace_id == *fixture.attribution().workspace_id()
            && entry.fact.attribution.project_id == *fixture.attribution().project_id()
            && entry.fact.attribution.repository_id == *fixture.attribution().repository_id()
            && entry.fact.attribution.delivery_id
                == Some(fixture.attribution().delivery_id().clone())
            && entry.fact.attribution.product_session_id
                == Some(fixture.attribution().product_session_id().clone())
            && entry.fact.attribution.user_id == *fixture.attribution().user_id()
            && entry.fact.measure == EnterpriseUsageMeasure::Publication
    }));

    Box::new(replayed).close().expect("close replayed storage");
    fs::remove_dir_all(root).expect("remove fixture");
}

fn temporary_root() -> PathBuf {
    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);
    std::env::temp_dir().join(format!(
        "winwincode-publication-enterprise-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed),
    ))
}
