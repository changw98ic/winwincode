// SPDX-License-Identifier: Apache-2.0

use crate::{action_enforcement::ActionEnforcementSigningKey, generated::ExecutionLeaseStamp};
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
    pub cost_microunits: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptAccountingStatement {
    pub lease: ExecutionLeaseStamp,
    pub manifest: Vec<String>,
    pub receipts: Vec<ProviderAccountingReceipt>,
    pub signature: Sha256Digest,
}

#[derive(Clone, Debug)]
pub struct VerifiedAccountingStatement(AttemptAccountingStatement);

impl VerifiedAccountingStatement {
    #[must_use]
    pub const fn statement(&self) -> &AttemptAccountingStatement {
        &self.0
    }
}

impl AttemptAccountingStatement {
    pub fn verified(
        &self,
        key: &ActionEnforcementSigningKey,
    ) -> Result<VerifiedAccountingStatement, &'static str> {
        self.verify(key)?;
        Ok(VerifiedAccountingStatement(self.clone()))
    }
    pub fn sign(&mut self, key: &ActionEnforcementSigningKey) -> Result<(), &'static str> {
        self.validate()?;
        self.signature = key.accounting_signature(&self.unsigned_bytes()?);
        self.encode()?;
        Ok(())
    }

    pub fn verify(&self, key: &ActionEnforcementSigningKey) -> Result<(), &'static str> {
        self.validate()?;
        self.encode()?;
        let expected = key.accounting_signature(&self.unsigned_bytes()?);
        if expected.0.len() != self.signature.0.len()
            || expected
                .0
                .bytes()
                .zip(self.signature.0.bytes())
                .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
                != 0
        {
            return Err("accounting signature rejected");
        }
        Ok(())
    }

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

    fn unsigned_bytes(&self) -> Result<Vec<u8>, &'static str> {
        serde_json::to_vec(&(&self.lease, &self.manifest, &self.receipts))
            .map_err(|_| "invalid accounting statement")
    }

    fn validate(&self) -> Result<(), &'static str> {
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
                || receipt
                    .cost_microunits
                    .is_some_and(|value| value > 9_007_199_254_740_991)
            {
                return Err("invalid Provider accounting receipt");
            }
        }
        Ok(())
    }
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
    fn signatures_bind_every_fact_and_reject_another_key() {
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
            receipts: Vec::new(),
            signature: Sha256Digest(String::new()),
        };
        let key = ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap();
        statement.sign(&key).unwrap();
        let mut decoded = AttemptAccountingStatement::decode(&statement.encode().unwrap()).unwrap();
        decoded.verify(&key).unwrap();
        assert!(
            decoded
                .verify(&ActionEnforcementSigningKey::from_bytes([8; 32]).unwrap())
                .is_err()
        );
        decoded
            .manifest
            .push("primary:mdl_00000000000000000000000002".into());
        assert!(decoded.verify(&key).is_err());
    }
}
