// SPDX-License-Identifier: Apache-2.0

//! Atomic promotion of the task graph sealed by the current solution review.
//!
//! This module is the only production path that turns planner proposals into
//! canonical [`DeliveryTask`] facts. The caller supplies neither tasks nor a
//! mutable Delivery snapshot.

use serde::{Deserialize, Serialize};
use winwincode_domain::DeliveryId;

use crate::domain::DeliverySpecId;

/// Immutable historical event retained for read-only projection/replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryTaskBreakdownApprovedEvent {
    pub schema_version: u8,
    pub delivery_id: DeliveryId,
    pub delivery_revision: u64,
    pub delivery_spec_id: DeliverySpecId,
    pub delivery_spec_revision: u64,
    pub review_set_sha256: String,
    pub tasks: Vec<crate::domain::DeliveryTask>,
}
