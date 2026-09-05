// SPDX-License-Identifier: Apache-2.0

//! Occupancy fencing enforcement on the device (CLIENT-300.3, plan 12.6
//! and the `client-control-port-v1.md` "Fencing 强制校验点" table).
//!
//! The Control Plane owns the occupancy lease and mints strictly increasing
//! fencing tokens; the Device Client durably mirrors the current
//! lease/token pair (the `occupancy_mirror` store row) and enforces it
//! locally. The plan fences exactly four command entry points —
//! [`FencedCommandKind`] — and the rule is absolute: the command's
//! occupancy stamp (lease id and fencing token) must equal the local
//! mirror exactly. Lower, higher, or foreign stamps are stale forever:
//! "旧 token 永远拒绝，避免网络重放让前任占用者复活".
//!
//! Only `client.occupancy.offer` and `client.occupancy.force_fence` advance
//! the mirror; each advance bumps the mirror revision, so every intent that
//! passed [`FencingGuard::authorize_command`] but has not executed yet is
//! invalidated. Callers re-check with [`FencingGuard::verify_ticket`] at
//! execution time — the check-then-execute window is exactly the replay
//! window the fencing exists to close.
//!
//! This module is pure: a [`FencingGuard`] is a snapshot of the mirror plus
//! verdict logic, with no I/O, so the semantic matrix is pinned by unit
//! tests here and the worker epic can call the entry points from any
//! context.

use crate::store::{DeviceStore, DeviceStoreError, OccupancyMirrorRecord};

/// The command entry points plan 12.6 fences on the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FencedCommandKind {
    /// `client.worker.launch` — spawn a managed worker process.
    WorkerLaunch,
    /// `client.worker.stop` — stop a supervised worker process.
    WorkerStop,
    /// `client.candidate.apply` — apply a candidate to a target branch.
    CandidateApply,
    /// Local repository registration/removal/change reported through
    /// `client.repository.upsert` / `client.repository.removed`.
    RepositoryMutation,
}

impl FencedCommandKind {
    /// Stable wire-facing name (audit logs, status text, tests).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::WorkerLaunch => "worker_launch",
            Self::WorkerStop => "worker_stop",
            Self::CandidateApply => "candidate_apply",
            Self::RepositoryMutation => "repository_mutation",
        }
    }
}

/// Why a fenced command or ticket was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FencingRejection {
    /// No occupancy mirror is persisted: the device holds no occupancy, so
    /// nothing can be authorized (fail closed, plan 18.3 recovery scan).
    MirrorNotSet,
    /// The stamp does not exactly match the mirror: a lower or higher token
    /// on the same lease, or a foreign lease id. "更低或不匹配的 token 一律
    /// 拒绝."
    StaleFencingToken,
    /// The ticket passed [`FencingGuard::authorize_command`], but the mirror
    /// advanced (an offer or force-fence overwrote it with a higher token)
    /// before the command executed. The earlier authorization is dead.
    SupersededIntent,
}

impl FencingRejection {
    /// The machine-readable `ClientControlPort` error code the wire surface
    /// carries for this rejection. A superseded intent and a stale token
    /// are indistinguishable from the outside: both mean "your stamp is no
    /// longer current"; a missing mirror means the device does not know the
    /// lease at all.
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

/// Verdict of [`FencingGuard::authorize_command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FencingVerdict {
    /// The stamp matched the mirror; [`FencingTicket`] is the receipt the
    /// command carries to its execution-time re-check.
    Authorized(FencingTicket),
    /// The command is refused before any local action.
    Rejected(FencingRejection),
}

/// One command's passing stamp, bound to the mirror revision observed at
/// authorization time.
///
/// The ticket is not a capability: it proves nothing on its own. The
/// execution site must re-validate it against the *current* mirror with
/// [`FencingGuard::verify_ticket`] immediately before acting, because any
/// mirror advance invalidates every outstanding ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FencingTicket {
    /// The command kind the stamp was checked for.
    pub kind: FencedCommandKind,
    /// The authorized lease id (equal to the mirror's).
    pub occupancy_lease_id: String,
    /// The authorized token (equal to the mirror's).
    pub occupancy_fencing_token: u64,
    /// Mirror revision observed at authorization time.
    pub mirror_revision: u64,
}

/// The pure fencing decision surface over one mirror snapshot.
///
/// Build it from the durable mirror with [`FencingGuard::from_store`] (or
/// from an already-loaded record with [`FencingGuard::new`]); the daemon
/// keeps one refreshed on every mirror advance. All methods are total and
/// side-effect free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FencingGuard {
    mirror: Option<OccupancyMirrorRecord>,
}

impl FencingGuard {
    /// Builds a guard over an explicit mirror snapshot (`None` = unset).
    #[must_use]
    pub const fn new(mirror: Option<OccupancyMirrorRecord>) -> Self {
        Self { mirror }
    }

    /// Reads the durable mirror and builds the guard from it — the
    /// integration point for embedders and the worker epic.
    ///
    /// # Errors
    ///
    /// Returns the store failure when the mirror read fails.
    pub fn from_store(store: &DeviceStore) -> Result<Self, DeviceStoreError> {
        Ok(Self::new(store.occupancy_mirror()?))
    }

    /// The mirror snapshot this guard decided from.
    #[must_use]
    pub const fn mirror(&self) -> Option<&OccupancyMirrorRecord> {
        self.mirror.as_ref()
    }

    /// The mirror revision of the snapshot, or `None` while unset.
    #[must_use]
    pub const fn mirror_revision(&self) -> Option<u64> {
        match self.mirror.as_ref() {
            Some(mirror) => Some(mirror.mirror_revision),
            None => None,
        }
    }

    /// The unified entry point every fenced command goes through (plan
    /// 12.6): worker launch, worker stop, candidate apply, and repository
    /// mutation all stamp their occupancy lease and token, and all four are
    /// judged by this one rule —
    ///
    /// - no mirror persisted → [`FencingRejection::MirrorNotSet`];
    /// - lease id **and** token exactly equal to the mirror →
    ///   [`FencingVerdict::Authorized`] with a revision-bound ticket;
    /// - anything else → [`FencingRejection::StaleFencingToken`] (a token
    ///   below the mirror, above it, or any foreign lease).
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

    /// Pure stamp equality against the mirror, for gating commands that are
    /// refused outright when the stamp is not current (e.g.
    /// `client.occupancy.release`). Returns the matched mirror.
    ///
    /// # Errors
    ///
    /// Returns [`FencingRejection::MirrorNotSet`] without a mirror and
    /// [`FencingRejection::StaleFencingToken`] for any mismatch.
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

    /// Re-validates a previously authorized ticket against this (current)
    /// snapshot immediately before execution — the invalidate semantics of
    /// "镜像更新后旧 token 的未处理命令立即失效": a mirror advance bumps the
    /// revision, so every ticket minted under the previous revision fails
    /// with [`FencingRejection::SupersededIntent`] even though its stamp
    /// once matched.
    ///
    /// # Errors
    ///
    /// Returns the rejection reason; `Ok(())` means the command may execute.
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

        // Same stamp, new revision (the force-fence re-fenced the same lease
        // at a higher token? No: the ticket's stamp still matches only if
        // the lease/token stayed equal — the revision bump alone already
        // kills the ticket, which is exactly the invalidate semantics).
        let same_stamp_new_revision = FencingGuard::new(Some(mirror("ocl_AAAA", 7, 3)));
        assert_eq!(
            same_stamp_new_revision.verify_ticket(&ticket),
            Err(FencingRejection::SupersededIntent)
        );

        // A force-fence that advanced the token also strands the ticket,
        // and now the stamp itself is stale too.
        let advanced = FencingGuard::new(Some(mirror("ocl_AAAA", 9, 3)));
        assert_eq!(
            advanced.verify_ticket(&ticket),
            Err(FencingRejection::StaleFencingToken)
        );

        // The current mirror still verifies its own fresh ticket.
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
