// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use winwincode_domain::{AttentionItemId, DeliveryId, StageRunId};

use super::{
    DeliverySpecId, DeliveryValidationError, DeliveryValidationErrorCode, MAX_TEXT_LENGTH,
    bounded_text, collection_length, duplicate_ids, nullable_text, portable_identifier,
    safe_non_negative, schema_version, validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttentionItemType {
    #[serde(rename = "requirement_question")]
    RequirementQuestion,
    #[serde(rename = "decision_required")]
    DecisionRequired,
    #[serde(rename = "verification_blocked")]
    VerificationBlocked,
    #[serde(rename = "scope_change")]
    ScopeChange,
    #[serde(rename = "delivery_approval")]
    DeliveryApproval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttentionItemStatus {
    #[serde(rename = "open")]
    Open,
    #[serde(rename = "resolved")]
    Resolved,
    #[serde(rename = "dismissed")]
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttentionOption {
    pub schema_version: u8,
    pub id: String,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttentionItem {
    pub schema_version: u8,
    pub id: AttentionItemId,
    pub delivery_id: DeliveryId,
    pub delivery_spec_id: DeliverySpecId,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub stage_run_id: Option<StageRunId>,
    #[serde(rename = "type")]
    pub item_type: AttentionItemType,
    pub title: String,
    pub context: String,
    pub options: Vec<AttentionOption>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub assigned_to: Option<String>,
    pub blocking: bool,
    pub status: AttentionItemStatus,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub resolution: Option<String>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub resolved_by: Option<String>,
    pub created_at_millis: u64,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    pub resolved_at_millis: Option<u64>,
}

pub(crate) fn validate(item: &AttentionItem, path: &str) -> Result<(), DeliveryValidationError> {
    schema_version(item.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&item.id.0, &format!("{path}.id"))?;
    portable_identifier(&item.delivery_id.0, &format!("{path}.deliveryId"))?;
    portable_identifier(&item.delivery_spec_id.0, &format!("{path}.deliverySpecId"))?;
    if let Some(run_id) = &item.stage_run_id {
        portable_identifier(&run_id.0, &format!("{path}.stageRunId"))?;
    }
    bounded_text(&item.title, &format!("{path}.title"), 256)?;
    bounded_text(&item.context, &format!("{path}.context"), MAX_TEXT_LENGTH)?;
    collection_length(item.options.len(), &format!("{path}.options"))?;
    for (index, option) in item.options.iter().enumerate() {
        let option_path = format!("{path}.options[{index}]");
        schema_version(
            option.schema_version,
            &format!("{option_path}.schemaVersion"),
        )?;
        portable_identifier(&option.id, &format!("{option_path}.id"))?;
        bounded_text(&option.label, &format!("{option_path}.label"), 256)?;
        bounded_text(
            &option.description,
            &format!("{option_path}.description"),
            MAX_TEXT_LENGTH,
        )?;
    }
    duplicate_ids(
        item.options.iter().map(|option| option.id.as_str()),
        &format!("{path}.options"),
    )?;
    nullable_text(
        item.assigned_to.as_deref(),
        &format!("{path}.assignedTo"),
        500,
    )?;
    nullable_text(
        item.resolution.as_deref(),
        &format!("{path}.resolution"),
        MAX_TEXT_LENGTH,
    )?;
    nullable_text(
        item.resolved_by.as_deref(),
        &format!("{path}.resolvedBy"),
        500,
    )?;
    safe_non_negative(item.created_at_millis, &format!("{path}.createdAtMillis"))?;
    if let Some(resolved) = item.resolved_at_millis {
        safe_non_negative(resolved, &format!("{path}.resolvedAtMillis"))?;
    }
    let resolution_complete = item.resolution.is_some()
        && item.resolved_by.is_some()
        && item
            .resolved_at_millis
            .is_some_and(|resolved| resolved >= item.created_at_millis);
    if (item.status == AttentionItemStatus::Open
        && (item.resolution.is_some()
            || item.resolved_by.is_some()
            || item.resolved_at_millis.is_some()))
        || (item.status != AttentionItemStatus::Open && !resolution_complete)
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            format!("{path}.status"),
            "attention resolution fields do not match its status",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use winwincode_domain::AttentionItemId;

    use crate::domain::{
        AttentionItem, AttentionItemStatus, AttentionItemType, Delivery, DeliveryStatus,
        test_fixture,
    };

    fn open_blocker() -> AttentionItem {
        let fixture = test_fixture();
        AttentionItem {
            schema_version: 3,
            id: AttentionItemId("attention-blocker".into()),
            delivery_id: fixture.id,
            delivery_spec_id: fixture.spec.id,
            stage_run_id: Some(fixture.stage_runs[0].id.clone()),
            item_type: AttentionItemType::VerificationBlocked,
            title: "Resolve verification".into(),
            context: "The current verdict has a blocker.".into(),
            options: vec![],
            assigned_to: Some("reviewer".into()),
            blocking: true,
            status: AttentionItemStatus::Open,
            resolution: None,
            resolved_by: None,
            created_at_millis: 1_800_000_000_020,
            resolved_at_millis: None,
        }
    }

    #[test]
    fn needs_attention_requires_open_blocking_attention() {
        let mut fixture = test_fixture();
        fixture.status = DeliveryStatus::NeedsAttention;
        fixture.verdict = None;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn passing_verdict_rejects_open_non_approval_blocker() {
        let mut fixture = test_fixture();
        fixture.attention_items.push(open_blocker());
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn delivered_delivery_rejects_open_blocking_attention() {
        let mut fixture = test_fixture();
        fixture.status = DeliveryStatus::Delivered;
        let mut blocker = open_blocker();
        blocker.item_type = AttentionItemType::DeliveryApproval;
        fixture.attention_items.push(blocker);
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }
}
