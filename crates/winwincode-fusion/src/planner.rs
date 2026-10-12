// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::knowledge::{
    ClaimNode, ClaimState, EvidenceDirection, EvidenceStrength, FusionEvidenceRecord,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvidenceLevel {
    LlmReasoning,
    CrossReview,
    Search,
    CodeGraph,
    StaticAnalysis,
    Test,
    Reproduction,
    Runtime,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationAction {
    pub provider: String,
    pub level: EvidenceLevel,
    pub question: String,
    pub capability: String,
    pub expected_information_gain: InformationGain,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InformationGain {
    High,
    Medium,
    Low,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvestigationPlan {
    pub claim_id: String,
    pub display_key: String,
    pub modes: Vec<InvestigationMode>,
    pub unknowns: Vec<String>,
    pub actions: Vec<InvestigationAction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InvestigationMode {
    EvidenceExpansion,
    Falsification,
    AssumptionAttack,
    CrossVerification,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCapability {
    pub provider: String,
    pub level: EvidenceLevel,
    pub capability: String,
    pub cost_rank: u8,
}

pub trait EvidenceProvider: Send + Sync + std::fmt::Debug {
    fn provider_id(&self) -> &'static str;
    fn can_handle(&self, action: &InvestigationAction) -> bool;
    fn investigate(
        &self,
        action: &InvestigationAction,
        claim: &ClaimNode,
    ) -> BoxFuture<'static, Result<Vec<FusionEvidenceRecord>, String>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InvestigationOutcome {
    SupportingEvidence,
    CounterEvidence,
    NegativeFinding,
    NoRelevantFinding,
    ProviderFailure,
    NotApplicable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderExecutionRecord {
    pub provider: String,
    pub capability: String,
    pub executed: bool,
    pub query: String,
    pub outcome: InvestigationOutcome,
    pub failure_reason: Option<String>,
    pub evidence_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClaimKind {
    NullSafety,
    SharedStateRace,
    Injection,
    Blocking,
    Style,
    Other,
}

#[must_use]
pub fn classify_claim_kind(claim: &ClaimNode) -> ClaimKind {
    let hay = format!(
        "{} {}",
        claim.display_key.to_ascii_lowercase(),
        claim.summary.to_ascii_lowercase()
    );
    if ["null", "unwrap", "optional", "panic"]
        .iter()
        .any(|n| hay.contains(n))
    {
        return ClaimKind::NullSafety;
    }
    if [
        "race",
        "shared",
        "fixture",
        "concurrent",
        "mutation",
        "lock",
        "map-race",
    ]
    .iter()
    .any(|n| hay.contains(n))
    {
        return ClaimKind::SharedStateRace;
    }
    if ["sql", "injection", "xss", "security"]
        .iter()
        .any(|n| hay.contains(n))
    {
        return ClaimKind::Injection;
    }
    if ["blocking", "merge", "ci", "quarantine"]
        .iter()
        .any(|n| hay.contains(n))
    {
        return ClaimKind::Blocking;
    }
    if ["style", "format", "naming"]
        .iter()
        .any(|n| hay.contains(n))
    {
        return ClaimKind::Style;
    }
    ClaimKind::Other
}

#[must_use]
pub fn provider_route_for(kind: ClaimKind) -> Vec<(&'static str, &'static str)> {
    match kind {
        ClaimKind::NullSafety => vec![
            ("ast", "nullable_dataflow"),
            ("codegraph", "find_callers"),
            ("static_analyzer", "null_guard"),
            ("test", "targeted_repro"),
            ("runtime", "panic_trace"),
        ],
        ClaimKind::SharedStateRace => vec![
            ("codegraph", "find_writers"),
            ("codegraph", "trace_ownership"),
            ("codegraph", "find_parallel_entrypoints"),
            ("codegraph", "find_sync"),
            ("static_analyzer", "race_or_null"),
            ("test", "targeted_repro"),
            ("runtime", "concurrency_trace"),
        ],
        ClaimKind::Injection => vec![
            ("ast", "dataflow"),
            ("static_analyzer", "taint"),
            ("test", "targeted_repro"),
        ],
        ClaimKind::Blocking => vec![
            ("ci", "config_and_logs"),
            ("test", "targeted_repro"),
            ("git", "history"),
        ],
        ClaimKind::Style | ClaimKind::Other => vec![
            ("lexical_search", "find_lock"),
            ("cross_review", "blind_review"),
        ],
    }
}

#[must_use]
pub fn plan_investigation(
    claim: &ClaimNode,
    capabilities: &[ProviderCapability],
) -> InvestigationPlan {
    let kind = classify_claim_kind(claim);
    let disputed = crate::knowledge::needs_investigation(claim);
    let mut modes = vec![
        InvestigationMode::EvidenceExpansion,
        InvestigationMode::Falsification,
        InvestigationMode::AssumptionAttack,
        InvestigationMode::CrossVerification,
    ];
    if !disputed {
        modes.retain(|mode| *mode == InvestigationMode::CrossVerification);
    }

    let mut unknowns = claim.unknowns.clone();
    if unknowns.is_empty() {
        unknowns.extend(default_unknowns(claim));
    }

    let mut actions = Vec::new();
    for (provider, capability) in provider_route_for(kind) {
        let gain = match capability {
            "trace_ownership"
            | "find_writers"
            | "find_parallel_entrypoints"
            | "nullable_dataflow"
            | "targeted_repro"
            | "find_sync" => InformationGain::High,
            "find_callers" | "race_or_null" | "null_guard" | "taint" | "dataflow" => {
                InformationGain::Medium
            }
            _ => InformationGain::Low,
        };
        let level = match provider {
            "codegraph" | "ast" | "lsp" => EvidenceLevel::CodeGraph,
            "static_analyzer" => EvidenceLevel::StaticAnalysis,
            "test" => EvidenceLevel::Test,
            "runtime" => EvidenceLevel::Runtime,
            "reproduction" => EvidenceLevel::Reproduction,
            "cross_review" => EvidenceLevel::CrossReview,
            _ => EvidenceLevel::Search,
        };
        push_action(
            &mut actions,
            capabilities,
            provider,
            level,
            capability,
            format!(
                "{capability} for {} ({:?}) unknowns={}",
                claim.display_key,
                kind,
                unknowns.len()
            ),
            gain,
        );
    }

    actions.sort_by_key(|action| {
        (
            action.expected_information_gain,
            action.level,
            action.provider.clone(),
        )
    });
    actions.dedup_by(|a, b| a.capability == b.capability && a.provider == b.provider);

    InvestigationPlan {
        claim_id: claim.id.clone(),
        display_key: claim.display_key.clone(),
        modes,
        unknowns,
        actions,
    }
}

fn push_action(
    actions: &mut Vec<InvestigationAction>,
    capabilities: &[ProviderCapability],
    provider: &str,
    level: EvidenceLevel,
    capability: &str,
    question: String,
    expected_information_gain: InformationGain,
) {
    let known = capabilities.iter().any(|cap| {
        cap.provider == provider && (cap.capability == capability || cap.capability == "*")
    });
    let _ = known;
    actions.push(InvestigationAction {
        provider: provider.to_owned(),
        level,
        question,
        capability: capability.to_owned(),
        expected_information_gain,
    });
}

fn is_shared_state_claim(claim: &ClaimNode) -> bool {
    let hay = format!(
        "{} {}",
        claim.display_key.to_ascii_lowercase(),
        claim.summary.to_ascii_lowercase()
    );
    [
        "race",
        "shared",
        "fixture",
        "mutation",
        "concurrent",
        "lock",
        "singleton",
    ]
    .iter()
    .any(|needle| hay.contains(needle))
}

fn default_unknowns(claim: &ClaimNode) -> Vec<String> {
    let mut unknowns = vec![
        format!("Is {} supported by a direct code path?", claim.display_key),
        format!(
            "Is there verified counter-evidence against {}?",
            claim.display_key
        ),
    ];
    if is_shared_state_claim(claim) {
        unknowns.push("Is the object a shared instance?".to_owned());
        unknowns.push("Do execution lifecycles overlap?".to_owned());
        unknowns.push("Is mutation synchronized?".to_owned());
    }
    unknowns
}

pub trait EvidencePlanner: Send + Sync + std::fmt::Debug {
    fn plan(&self, claim: &ClaimNode, capabilities: &[ProviderCapability]) -> InvestigationPlan;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultEvidencePlanner;

impl EvidencePlanner for DefaultEvidencePlanner {
    fn plan(&self, claim: &ClaimNode, capabilities: &[ProviderCapability]) -> InvestigationPlan {
        plan_investigation(claim, capabilities)
    }
}

pub async fn execute_plan_step(
    providers: &[Arc<dyn EvidenceProvider>],
    action: &InvestigationAction,
    claim: &ClaimNode,
) -> Result<Vec<FusionEvidenceRecord>, String> {
    for provider in providers {
        if provider.can_handle(action) {
            return provider.investigate(action, claim).await;
        }
    }
    Err(format!(
        "no EvidenceProvider for {}::{}",
        action.provider, action.capability
    ))
}

pub fn merge_evidence(
    claim: &mut ClaimNode,
    records: &mut Vec<FusionEvidenceRecord>,
    mut incoming: Vec<FusionEvidenceRecord>,
) {
    for record in &mut incoming {
        if record.id.is_empty() {
            record.id = format!("ev_{}_{}", claim.id, records.len());
        }
        record.claim_id.clone_from(&claim.id);
        match record.direction {
            EvidenceDirection::Support => claim.evidence_ids.push(record.id.clone()),
            EvidenceDirection::Counter => {
                claim.counter_evidence_ids.push(record.id.clone());
            }
        }
        records.push(record.clone());
    }
}

#[must_use]
pub fn recompute_state(claim: &ClaimNode) -> ClaimState {
    if claim.state == ClaimState::Confirmed || claim.state == ClaimState::Refuted {
        return claim.state;
    }
    if claim.has_verified_counter && claim.evidence_ids.is_empty() {
        return ClaimState::Disputed;
    }
    if claim.supporter_count > 0 && claim.opponent_count > 0 {
        return ClaimState::Disputed;
    }
    if claim.supporter_count > 0 {
        return ClaimState::Supported;
    }
    ClaimState::Discovered
}

#[must_use]
pub fn strongest_evidence(
    records: &[FusionEvidenceRecord],
    claim_id: &str,
) -> Option<EvidenceStrength> {
    records
        .iter()
        .filter(|record| record.claim_id == claim_id && !record.invalidated)
        .map(|record| record.strength)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{ClaimIdentity, ClaimState};

    fn disputed_claim() -> ClaimNode {
        ClaimNode {
            id: "c_test".to_owned(),
            identity: ClaimIdentity::parse("root:shared-fixture-race"),
            display_key: "claim:root:shared-fixture-race".to_owned(),
            summary: "Root cause is shared module-level cart mutated across parallel tests"
                .to_owned(),
            state: ClaimState::Disputed,
            supporters: vec!["mimo".to_owned()],
            opponents: vec!["glm".to_owned(), "deepseek".to_owned()],
            evidence_ids: Vec::new(),
            counter_evidence_ids: Vec::new(),
            unknowns: Vec::new(),
            supporter_count: 1,
            opponent_count: 2,
            has_verified_counter: false,
        }
    }

    #[test]
    fn planner_emits_escalation_for_shared_state_dispute() {
        let plan = plan_investigation(&disputed_claim(), &[]);
        assert!(plan.modes.contains(&InvestigationMode::Falsification));
        assert!(
            plan.actions
                .iter()
                .any(|action| action.capability == "trace_ownership"
                    && action.level == EvidenceLevel::CodeGraph)
        );
        assert!(
            plan.actions
                .iter()
                .any(|action| action.capability == "targeted_repro")
        );
        assert!(
            plan.actions
                .iter()
                .any(|action| action.level >= EvidenceLevel::CodeGraph)
        );
    }

    #[test]
    fn merge_evidence_records_counter_without_self_verifying_it() {
        let mut claim = disputed_claim();
        let mut store = Vec::new();
        merge_evidence(
            &mut claim,
            &mut store,
            vec![FusionEvidenceRecord {
                id: "ev_x".to_owned(),
                claim_id: String::new(),
                provider: "codegraph".to_owned(),
                direction: EvidenceDirection::Counter,
                kind: "shared_mutation_path".to_owned(),
                strength: EvidenceStrength::Direct,
                facts: vec!["workers use distinct instances".to_owned()],
                source_refs: vec!["codegraph:ownership".to_owned()],
                independence_group: "ig:x".to_owned(),
                verified: true,
                invalidated: false,
            }],
        );
        assert!(!claim.has_verified_counter);
        assert_eq!(claim.counter_evidence_ids, vec!["ev_x".to_owned()]);
    }
}

#[derive(Debug)]
pub struct CodeGraphEvidenceProvider {
    backend: Arc<dyn CodeGraphEvidenceBackend>,
}

pub trait CodeGraphEvidenceBackend: Send + Sync + std::fmt::Debug {
    fn find_writers(&self, symbol: &str) -> BoxFuture<'static, Result<Vec<String>, String>>;
    fn trace_ownership(&self, symbol: &str) -> BoxFuture<'static, Result<Vec<String>, String>>;
    fn find_parallel_entrypoints(
        &self,
        symbol: &str,
    ) -> BoxFuture<'static, Result<Vec<String>, String>>;
    fn find_readers(&self, symbol: &str) -> BoxFuture<'static, Result<Vec<String>, String>>;
    fn find_callers(&self, symbol: &str) -> BoxFuture<'static, Result<Vec<String>, String>>;
    fn find_sync(&self, symbol: &str) -> BoxFuture<'static, Result<Vec<String>, String>>;
}

#[derive(Debug, Clone)]
pub struct CliCodeGraphBackend {
    pub project_path: String,
    pub codegraph_bin: String,
}

impl CliCodeGraphBackend {
    #[must_use]
    pub fn new(project_path: impl Into<String>) -> Self {
        Self {
            project_path: project_path.into(),
            codegraph_bin: std::env::var("CODEGRAPH_BIN")
                .unwrap_or_else(|_| "codegraph".to_owned()),
        }
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let output = std::process::Command::new(&self.codegraph_bin)
            .args(args)
            .current_dir(&self.project_path)
            .output()
            .map_err(|error| format!("codegraph spawn failed: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "codegraph {} failed: {}",
                args.first().unwrap_or(&""),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl CodeGraphEvidenceBackend for CliCodeGraphBackend {
    fn find_writers(&self, symbol: &str) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let this = self.clone();
        let symbol = symbol.to_owned();
        Box::pin(async move {
            let callers = this
                .run(&["callers", "--json", &symbol])
                .or_else(|_| this.run(&["callers", &symbol]))?;
            let mut facts = Vec::new();
            for line in callers.lines() {
                let line = line.trim();
                if !line.is_empty() {
                    facts.push(format!("writer/caller: {line}"));
                }
            }
            if facts.is_empty() {
                facts.push(format!("codegraph callers: no writer for symbol {symbol}"));
            }
            Ok(facts)
        })
    }

    fn trace_ownership(&self, symbol: &str) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let this = self.clone();
        let symbol = symbol.to_owned();
        Box::pin(async move {
            let impact = this.run(&["impact", &symbol]).unwrap_or_default();
            let mut facts = vec![format!("ownership/impact for {symbol}:")];
            for line in impact.lines().take(20) {
                let line = line.trim();
                if !line.is_empty() {
                    facts.push(line.to_owned());
                }
            }
            if let Ok(query) = this.run(&["query", "--limit", "5", &symbol]) {
                for line in query.lines().take(5) {
                    let line = line.trim();
                    if !line.is_empty() {
                        facts.push(format!("symbol: {line}"));
                    }
                }
            }
            if facts.iter().any(|fact| fact.contains("distinct instance")) {
                facts.push("ownership suggests distinct instance per consumer".to_owned());
            }
            Ok(facts)
        })
    }

    fn find_parallel_entrypoints(
        &self,
        symbol: &str,
    ) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let this = self.clone();
        let symbol = symbol.to_owned();
        Box::pin(async move {
            let callers = this.run(&["callers", &symbol]).unwrap_or_default();
            let mut facts = vec![format!("parallel-entry check for {symbol}")];
            let mut count = 0usize;
            for line in callers.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                count += 1;
                facts.push(format!("entry: {line}"));
            }
            facts.push(format!(
                "distinct caller symbols={count} (>=2 may allow concurrent entry)"
            ));
            Ok(facts)
        })
    }

    fn find_readers(&self, symbol: &str) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let this = self.clone();
        let symbol = symbol.to_owned();
        Box::pin(async move {
            let callees = this.run(&["callees", &symbol]).unwrap_or_default();
            let mut facts = vec![format!("readers/callees for {symbol}")];
            for line in callees.lines().take(20) {
                let line = line.trim();
                if !line.is_empty() {
                    facts.push(line.to_owned());
                }
            }
            Ok(facts)
        })
    }

    fn find_callers(&self, symbol: &str) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let this = self.clone();
        let symbol = symbol.to_owned();
        Box::pin(async move {
            let callers = this.run(&["callers", &symbol]).unwrap_or_default();
            let mut facts = vec![format!("callers for {symbol}")];
            for line in callers.lines().take(30) {
                let line = line.trim();
                if !line.is_empty() {
                    facts.push(line.to_owned());
                }
            }
            Ok(facts)
        })
    }

    fn find_sync(&self, symbol: &str) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let this = self.clone();
        let symbol = symbol.to_owned();
        Box::pin(async move {
            let query = this.run(&["query", "--limit", "10", "lock mutex sync RwLock"]);
            let mut facts = vec![format!("sync search around {symbol}")];
            match query {
                Ok(text) => {
                    let hits = text.lines().filter(|line| !line.trim().is_empty()).count();
                    if hits == 0 {
                        facts.push(
                            "NEGATIVE_FINDING: no lock/mutex/sync symbols in index".to_owned(),
                        );
                    } else {
                        for line in text.lines().take(10) {
                            let line = line.trim();
                            if !line.is_empty() {
                                facts.push(format!("sync-symbol: {line}"));
                            }
                        }
                    }
                }
                Err(error) => facts.push(format!("sync search failed: {error}")),
            }
            Ok(facts)
        })
    }
}

impl CodeGraphEvidenceProvider {
    #[must_use]
    pub fn new(backend: Arc<dyn CodeGraphEvidenceBackend>) -> Self {
        Self { backend }
    }

    #[must_use]
    pub fn cli(project_path: impl Into<String>) -> Self {
        Self::new(Arc::new(CliCodeGraphBackend::new(project_path)))
    }
}

impl EvidenceProvider for CodeGraphEvidenceProvider {
    fn provider_id(&self) -> &'static str {
        "codegraph"
    }

    fn can_handle(&self, action: &InvestigationAction) -> bool {
        action.provider == "codegraph"
            && matches!(
                action.capability.as_str(),
                "trace_ownership" | "find_writers" | "find_parallel_entrypoints"
            )
    }

    fn investigate(
        &self,
        action: &InvestigationAction,
        claim: &ClaimNode,
    ) -> BoxFuture<'static, Result<Vec<FusionEvidenceRecord>, String>> {
        let backend = Arc::clone(&self.backend);
        let claim_id = claim.id.clone();
        let display_key = claim.display_key.clone();
        let capability = action.capability.clone();
        let symbol = claim.identity.key.clone();
        Box::pin(async move {
            let facts = match capability.as_str() {
                "trace_ownership" => backend.trace_ownership(&symbol).await?,
                "find_writers" => backend.find_writers(&symbol).await?,
                "find_parallel_entrypoints" => backend.find_parallel_entrypoints(&symbol).await?,
                other => return Err(format!("unsupported codegraph capability: {other}")),
            };
            if facts.is_empty() {
                return Ok(Vec::new());
            }
            let direction = if capability == "trace_ownership"
                && facts.iter().any(|fact| fact.contains("distinct instance"))
            {
                EvidenceDirection::Counter
            } else {
                EvidenceDirection::Support
            };
            Ok(vec![FusionEvidenceRecord {
                id: format!("ev_cg_{claim_id}_{capability}"),
                claim_id,
                provider: "codegraph".to_owned(),
                direction,
                kind: capability.clone(),
                strength: EvidenceStrength::Direct,
                facts,
                source_refs: vec![format!("codegraph:{capability}:{display_key}")],
                independence_group: format!("ig:codegraph:{capability}:{display_key}"),
                verified: true,
                invalidated: false,
            }])
        })
    }
}
