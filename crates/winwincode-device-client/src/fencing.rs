use crate::store::{DeviceStore, DeviceStoreError, OccupancyMirrorRecord};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FencedCommandKind {
    WorkerLaunch,
    WorkerStop,
    CandidateApply,
    RepositoryMutation,
    ManagedApp,
}

impl FencedCommandKind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::WorkerLaunch => "worker_launch",
            Self::WorkerStop => "worker_stop",
            Self::CandidateApply => "candidate_apply",
            Self::RepositoryMutation => "repository_mutation",
            Self::ManagedApp => "managed_app",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FencingRejection {
    MirrorNotSet,
    StaleFencingToken,
    SupersededIntent,
}

impl FencingRejection {
    #[must_use]
    pub const fn wire_error_code(self) -> winwincode_client_port::domain::ClientControlErrorCode {
        use winwincode_client_port::domain::ClientControlErrorCode;
        match self {
            Self::MirrorNotSet => ClientControlErrorCode::UnknownLease,
            Self::StaleFencingToken | Self::SupersededIntent => {
                ClientControlErrorCode::StaleFencingToken
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FencingVerdict {
    Authorized(FencingTicket),
    Rejected(FencingRejection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FencingTicket {
    pub kind: FencedCommandKind,
    pub occupancy_lease_id: String,
    pub occupancy_fencing_token: u64,
    pub mirror_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FencingGuard {
    mirror: Option<OccupancyMirrorRecord>,
}

impl FencingGuard {
    #[must_use]
    pub const fn new(mirror: Option<OccupancyMirrorRecord>) -> Self {
        Self { mirror }
    }

    pub fn from_store(store: &DeviceStore) -> Result<Self, DeviceStoreError> {
        Ok(Self::new(store.occupancy_mirror()?))
    }

    #[must_use]
    pub const fn mirror(&self) -> Option<&OccupancyMirrorRecord> {
        self.mirror.as_ref()
    }

    #[must_use]
    pub const fn mirror_revision(&self) -> Option<u64> {
        match self.mirror.as_ref() {
            Some(mirror) => Some(mirror.mirror_revision),
            None => None,
        }
    }

    #[must_use]
    pub fn authorize_command(
        &self,
        kind: FencedCommandKind,
        occupancy_lease_id: &str,
        occupancy_fencing_token: u64,
    ) -> FencingVerdict {
        match self.check_stamp(occupancy_lease_id, occupancy_fencing_token) {
            Ok(mirror) => FencingVerdict::Authorized(FencingTicket {
                kind,
                occupancy_lease_id: mirror.occupancy_lease_id.clone(),
                occupancy_fencing_token: mirror.fencing_token,
                mirror_revision: mirror.mirror_revision,
            }),
            Err(rejection) => FencingVerdict::Rejected(rejection),
        }
    }

    pub fn check_stamp(
        &self,
        occupancy_lease_id: &str,
        occupancy_fencing_token: u64,
    ) -> Result<&OccupancyMirrorRecord, FencingRejection> {
        let Some(mirror) = self.mirror.as_ref() else {
            return Err(FencingRejection::MirrorNotSet);
        };
        if mirror.occupancy_lease_id == occupancy_lease_id
            && mirror.fencing_token == occupancy_fencing_token
        {
            Ok(mirror)
        } else {
            Err(FencingRejection::StaleFencingToken)
        }
    }

    pub fn verify_ticket(&self, ticket: &FencingTicket) -> Result<(), FencingRejection> {
        let mirror =
            self.check_stamp(&ticket.occupancy_lease_id, ticket.occupancy_fencing_token)?;
        if mirror.mirror_revision != ticket.mirror_revision {
            return Err(FencingRejection::SupersededIntent);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirror(lease: &str, token: u64, revision: u64) -> OccupancyMirrorRecord {
        OccupancyMirrorRecord {
            occupancy_lease_id: lease.to_owned(),
            fencing_token: token,
            holder_user_id: Some("usr_01j2".to_owned()),
            mirror_revision: revision,
            claim_request_id: Some("ocq_CCCCCCCCCCCCCCCCCCCCCCCCCC".to_owned()),
            idle_expires_at: None,
            acknowledged_at: "2026-09-04T00:00:00.000Z".to_owned(),
            updated_at: "2026-09-04T00:00:00.000Z".to_owned(),
        }
    }

    #[test]
    fn authorize_command_passes_only_an_exact_stamp_match() {
        let guard = FencingGuard::new(Some(mirror("ocl_AAAA", 7, 2)));
        for kind in [
            FencedCommandKind::WorkerLaunch,
            FencedCommandKind::WorkerStop,
            FencedCommandKind::CandidateApply,
            FencedCommandKind::RepositoryMutation,
        ] {
            let verdict = guard.authorize_command(kind, "ocl_AAAA", 7);
            let FencingVerdict::Authorized(ticket) = verdict else {
                panic!("{kind:?}: the exact stamp must authorize");
            };
            assert_eq!(ticket.kind, kind);
            assert_eq!(ticket.occupancy_lease_id, "ocl_AAAA");
            assert_eq!(ticket.occupancy_fencing_token, 7);
            assert_eq!(ticket.mirror_revision, 2);
        }
    }

    #[test]
    fn authorize_command_rejects_stale_lower_and_foreign_tokens() {
        let guard = FencingGuard::new(Some(mirror("ocl_AAAA", 7, 2)));
        let stale = [
            ("lower token", "ocl_AAAA", 6),
            ("much older token", "ocl_AAAA", 1),
            ("higher unseen token", "ocl_AAAA", 8),
            ("foreign lease", "ocl_BBBB", 7),
            ("foreign lease and token", "ocl_BBBB", 9),
        ];
        for (label, lease, token) in stale {
            for kind in [
                FencedCommandKind::WorkerLaunch,
                FencedCommandKind::WorkerStop,
                FencedCommandKind::CandidateApply,
                FencedCommandKind::RepositoryMutation,
            ] {
                let verdict = guard.authorize_command(kind, lease, token);
                assert_eq!(
                    verdict,
                    FencingVerdict::Rejected(FencingRejection::StaleFencingToken),
                    "{label} must reject {kind:?}"
                );
            }
        }
    }

    #[test]
    fn authorize_command_without_a_mirror_fails_closed() {
        let guard = FencingGuard::new(None);
        assert_eq!(
            guard.authorize_command(FencedCommandKind::WorkerLaunch, "ocl_AAAA", 7),
            FencingVerdict::Rejected(FencingRejection::MirrorNotSet)
        );
        assert!(guard.mirror_revision().is_none());
        assert_eq!(
            guard.check_stamp("ocl_AAAA", 7),
            Err(FencingRejection::MirrorNotSet)
        );
    }

    #[test]
    fn a_mirror_advance_invalidates_outstanding_tickets() {
        let before = FencingGuard::new(Some(mirror("ocl_AAAA", 7, 2)));
        let ticket = match before.authorize_command(FencedCommandKind::WorkerLaunch, "ocl_AAAA", 7)
        {
            FencingVerdict::Authorized(ticket) => ticket,
            FencingVerdict::Rejected(rejection) => panic!("must authorize: {rejection:?}"),
        };

        let same_stamp_new_revision = FencingGuard::new(Some(mirror("ocl_AAAA", 7, 3)));
        assert_eq!(
            same_stamp_new_revision.verify_ticket(&ticket),
            Err(FencingRejection::SupersededIntent)
        );

        let advanced = FencingGuard::new(Some(mirror("ocl_AAAA", 9, 3)));
        assert_eq!(
            advanced.verify_ticket(&ticket),
            Err(FencingRejection::StaleFencingToken)
        );

        let fresh = match advanced.authorize_command(FencedCommandKind::WorkerStop, "ocl_AAAA", 9) {
            FencingVerdict::Authorized(ticket) => ticket,
            FencingVerdict::Rejected(rejection) => panic!("must authorize: {rejection:?}"),
        };
        assert_eq!(advanced.verify_ticket(&fresh), Ok(()));
        assert_eq!(
            before.verify_ticket(&fresh),
            Err(FencingRejection::StaleFencingToken)
        );
    }

    #[test]
    fn verify_ticket_fails_closed_when_the_mirror_disappears() {
        let guard = FencingGuard::new(Some(mirror("ocl_AAAA", 7, 1)));
        let ticket = match guard.authorize_command(FencedCommandKind::CandidateApply, "ocl_AAAA", 7)
        {
            FencingVerdict::Authorized(ticket) => ticket,
            FencingVerdict::Rejected(rejection) => panic!("must authorize: {rejection:?}"),
        };
        let empty = FencingGuard::new(None);
        assert_eq!(
            empty.verify_ticket(&ticket),
            Err(FencingRejection::MirrorNotSet)
        );
    }

    #[test]
    fn rejections_map_onto_wire_error_codes() {
        assert_eq!(
            FencingRejection::MirrorNotSet.wire_error_code(),
            winwincode_client_port::domain::ClientControlErrorCode::UnknownLease
        );
        assert_eq!(
            FencingRejection::StaleFencingToken.wire_error_code(),
            winwincode_client_port::domain::ClientControlErrorCode::StaleFencingToken
        );
        assert_eq!(
            FencingRejection::SupersededIntent.wire_error_code(),
            winwincode_client_port::domain::ClientControlErrorCode::StaleFencingToken
        );
    }

    #[test]
    fn fenced_command_kinds_have_stable_names() {
        assert_eq!(FencedCommandKind::WorkerLaunch.name(), "worker_launch");
        assert_eq!(FencedCommandKind::WorkerStop.name(), "worker_stop");
        assert_eq!(FencedCommandKind::CandidateApply.name(), "candidate_apply");
        assert_eq!(
            FencedCommandKind::RepositoryMutation.name(),
            "repository_mutation"
        );
    }
}
