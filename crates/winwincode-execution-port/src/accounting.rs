// SPDX-License-Identifier: Apache-2.0

use crate::generated::ExecutionLeaseStamp;
use serde::{Deserialize, Serialize};
use winwincode_domain::{ModelExchangeId, Sha256Digest};

pub const MAX_ACCOUNTING_STATEMENT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingAccountingPage {
    pub leases: Vec<ExecutionLeaseStamp>,
    pub next_offset: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderAccountingReceipt {
    pub slot: String,
    pub model_exchange_id: ModelExchangeId,
    pub provider_id: String,
    pub provider_receipt_id: String,
    pub source_digest: Sha256Digest,
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub known_tokens: u64,
    pub cost_microunits: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptAccountingStatement {
    pub lease: ExecutionLeaseStamp,
    pub manifest: Vec<String>,
    pub receipts: Vec<ProviderAccountingReceipt>,
}

impl AttemptAccountingStatement {
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        let bytes = serde_json::to_vec(self).map_err(|_| "invalid accounting encoding")?;
        if bytes.len() > MAX_ACCOUNTING_STATEMENT_BYTES {
            return Err("accounting statement too large");
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.is_empty() || bytes.len() > MAX_ACCOUNTING_STATEMENT_BYTES {
            return Err("accounting statement too large");
        }
        let statement: Self =
            serde_json::from_slice(bytes).map_err(|_| "invalid accounting statement")?;
        statement.validate()?;
        Ok(statement)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.lease.attempt < 1
            || self.manifest.is_empty()
            || self.manifest.len() > 4096
            || self.receipts.len() > self.manifest.len()
            || !self.manifest.windows(2).all(|pair| pair[0] < pair[1])
        {
            return Err("invalid accounting manifest");
        }
        let mut slots = std::collections::BTreeSet::new();
        for slot in &self.manifest {
            if !token(slot, 256) {
                return Err("invalid accounting slot");
            }
        }
        for receipt in &self.receipts {
            if self.manifest.binary_search(&receipt.slot).is_err()
                || !slots.insert(&receipt.slot)
                || !receipt.model_exchange_id.0.starts_with("mdl_")
                || !token(&receipt.provider_id, 128)
                || !token(&receipt.provider_receipt_id, 256)
                || !digest(&receipt.source_digest.0)
                || receipt
                    .tokens
                    .is_some_and(|value| value > 9_007_199_254_740_991)
                || receipt.known_tokens > 9_007_199_254_740_991
                || receipt
                    .tokens
                    .is_some_and(|tokens| tokens < receipt.known_tokens)
                || receipt
                    .cost_microunits
                    .is_some_and(|value| value > 9_007_199_254_740_991)
            {
                return Err("invalid Provider accounting receipt");
            }
        }
        self.encode()?;
        Ok(())
    }
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive a borrowed field"
)]
const fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn token(value: &str, bound: usize) -> bool {
    !value.is_empty() && value.len() <= bound && !value.chars().any(char::is_control)
}
fn digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounting_round_trip_preserves_lower_bounds_and_rejects_invalid_totals() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let lease = fixture["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|message| message.get("lease"))
            .unwrap();
        let mut statement = AttemptAccountingStatement {
            lease: serde_json::from_value(lease.clone()).unwrap(),
            manifest: vec!["primary:mdl_00000000000000000000000001".into()],
            receipts: vec![ProviderAccountingReceipt {
                slot: "primary:mdl_00000000000000000000000001".into(),
                model_exchange_id: ModelExchangeId("mdl_00000000000000000000000001".into()),
                provider_id: "provider".into(),
                provider_receipt_id: "receipt".into(),
                source_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
                tokens: None,
                known_tokens: 5,
                cost_microunits: None,
            }],
        };
        statement.validate().unwrap();
        assert_eq!(
            AttemptAccountingStatement::decode(&statement.encode().unwrap()).unwrap(),
            statement
        );
        statement.receipts[0].tokens = Some(4);
        assert!(statement.validate().is_err());
        statement.receipts[0].tokens = None;
        statement.receipts[0].known_tokens = 9_007_199_254_740_992;
        assert!(statement.validate().is_err());
    }
}
