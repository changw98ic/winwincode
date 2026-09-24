# ADR-0037：Agent Fusion Engine——以 Claim 为中心的动态证据调查

- 状态：已接受
- 日期：2026-09-22
- 对应任务：`winwincode-community.5`、`winwincode-community.5.14`
- 记忆键：`fusion-agent-engine-20260922`
- 关联：[ADR-0035](0035-jev-strong-judge-verification-scheduler.md)、[ADR-0036](0036-multi-round-fusion-convergence.md)
- 回归样例：`claim:root-shared-fixture-race`、`claim:defect-null-unwrap`、`claim:blocking-ci`

## 定义

> Fusion 是一个以 Claim 为中心的动态多 Agent 调查系统：先通过异构模型扩大候选知识集合，再针对冲突主动调用 CodeGraph、搜索、静态分析、测试和运行时工具获取新的事实，通过多轮交叉验证逐步逼近 Oracle Union，而不是通过投票或一次性裁决选择某个模型的答案。

```text
Fusion ≠ Voting
Fusion ≠ Judge(A, B)
Fusion ≠ Majority Consensus

Fusion
=
Independent Discovery
+ Knowledge Union
+ Conflict Discovery
+ Active Evidence Search
+ Adversarial Verification
+ Deterministic Verification
+ Final Synthesis
```

## 五条顶层原则（硬约束）

```text
1. Fusion is additive by default.                    默认做加法
2. Disagreement triggers investigation, not elimination.
3. No new evidence triggers escalation, not termination.
4. Only verified contradiction can subtract a supported claim.
5. Consensus is metadata, not evidence.              3 个模型一句话 ≠ 3 份证据
```

## 架构（非固定流水线）

每个 Claim 独立进入 investigation branch，形成**动态证据搜索图**：

```text
User Task → Context Builder → R1 Independent Exploration
  → Claim Extraction → Claim Normalization / Union → Conflict Graph
      ├─ resolved ─────────────────────────────────┐
      └─ disputed → Evidence Planner → Evidence Providers
                      (CodeGraph/Search/AST/LSP/Git/Test/Runtime)
                    → Evidence Store → Adversarial Cross Review
                    → Independent JEV → Claim State Recompute
  → Final Claim Graph → Final Synthesis
```

示例：

```text
Claim A → CodeGraph → confirmed
Claim B → targeted test → refuted
Claim C → Git history → confirmed
Claim D → 证据不足 → unresolved
```

## R0 输入规范化

```text
claim:defect-null-unwrap / defect-null-unwrap / CLAIM:defect-null-unwrap
  → namespace=defect, key=null-unwrap, canonicalKey=defect:null-unwrap
```

内部：

```json
{ "claimId": "c_123", "namespace": "defect", "key": "null-unwrap" }
```

展示：`claim:defect-null-unwrap`。禁止让字符串 `"claim:"` 承担类型系统职责。

（产品已有 `claim_group_key` 作为第一版；升级为 `ClaimIdentity`。）

## R1 Independent Discovery

唯一目标：**最大化 Recall，不追求共识**。互相不可见、同任务同资料、独立分析。

```json
{
  "claims": [{
    "key": "defect:shared-map-race",
    "verdict": "present",
    "confidence": 0.81,
    "evidence": [],
    "reasoningSummary": "...",
    "unknowns": []
  }]
}
```

`confidence` 只是 model self-confidence，仅作 metadata，不当事实。

## R2 Claim Union

```text
ClaimSet = Union(M1..Mn)
```

至少一模型提出、非解析错误 → 进入 Claim Graph。禁止用多数票淘汰。

## Claim Graph（每个 Claim）

```text
proponents / opponents / evidences / counterEvidences
assumptions / unknowns / dependencies / contradictions
investigationHistory / verificationHistory / state
```

## Claim 状态机

```text
DISCOVERED → SUPPORTED → CONFIRMED
                      ↘ DISPUTED → INVESTIGATING
                                      ├→ CONFIRMED
                                      ├→ REFUTED（必须 verified counter-evidence）
                                      └→ DISPUTED → ESCALATED
                                                     ├→ CONFIRMED
                                                     └→ UNRESOLVED
```

`REFUTED` 硬条件：**存在 verified counter-evidence**。
禁止因「3 个模型反对」进入 REFUTED。

## 争议检测（不只看 2:2 / 2:3）

进入 disputed 当：

- 支持与反对同时存在；或
- evidence 互相冲突；或
- 重要独见未验证；或
- 对最终结果影响高但证据不足。

**4:1 也可以 disputed**——若唯一反对者拿出 reproduction / runtime trace / 直接代码路径，而四家只是「我觉得不是」。

## R3 Evidence Planner（核心新组件）

```yaml
claim: root:shared-fixture-race
questions:
  - fixture 是否 singleton/shared
  - 两执行路径是否可并发
  - 是否 concurrent mutation
  - 是否有同步保护
actions:
  - codegraph.trace_ownership
  - codegraph.find_callers
  - codegraph.find_writers
  - search.find_lock
  - runtime.inspect_execution
  - test.generate_race_reproduction
```

## Evidence Acquisition（EvidenceProvider）

首批：`lexical_search` `semantic_search` **`codegraph`** `ast` `lsp` `git` `test` `runtime` `reproduction` `static_analyzer` `ci`。

CodeGraph 定位：结构化 Evidence Provider（call path / ownership / dependency / shared state / lifecycle / mutation path），不是自由搜索框。

建议补齐查询：

```text
find_writers / trace_ownership / find_mutation_paths / find_parallel_entrypoints
```

CodeGraph 必须返回 Evidence 而非裸图：

```json
{
  "evidenceId": "ev_302",
  "provider": "codegraph",
  "claimKey": "root:shared-fixture-race",
  "evidenceType": "shared_mutation_path",
  "facts": ["WorkerA reaches Fixture.mutate", "WorkerB reaches Fixture.mutate"],
  "strength": "direct",
  "reproducible": true
}
```

模型只能解释 evidence，不能凭空篡改。

## Evidence 模型

```json
{
  "id": "ev_xxx",
  "claimKey": "...",
  "provider": "codegraph",
  "direction": "support|counter",
  "kind": "call_path",
  "strength": "DIRECT|STRONG_INFERENCE|WEAK_INFERENCE|SPECULATION",
  "facts": [],
  "sourceRefs": [],
  "derivedFrom": [],
  "independenceGroup": "...",
  "verified": false,
  "invalidated": false
}
```

禁止伪精度分数（0.8735 之类）。

## Evidence 去重

按 semantic similarity + source overlap + reasoning premise overlap 聚成 `independenceGroup` / Evidence Cluster。
三家基于同一错误假设的口述 = 1 个独立组，不是 3 votes。

## R3 不只是「找新证据」

| 模式 | 作用 |
|---|---|
| Evidence Expansion | 找新证据 |
| Falsification | 主动推翻 Claim |
| Assumption Attack | 攻击双方错误前提 |
| Cross Verification | 交叉验证已有 Evidence |

## Cross Review

E1 来自 MiMo → 交给 OpenCode 盲审 + CodeGraph 验 ownership。
「MiMo 推理 + OpenCode 盲审 + CodeGraph 直接证据」强于「三家口头反对」。

## 验证升级阶梯（无新证据 ≠ 结束）

```text
L0 LLM reasoning
L1 cross-model inspection
L2 text/semantic search
L3 CodeGraph / AST / LSP
L4 static analysis
L5 targeted tests
L6 reproduction
L7 runtime evidence
```

## R4 JEV 真正角色

输入：Claim + verified support/counter + unverified support/counter + assumptions + unknowns。
**不提供人数；不提供模型名**（防品牌先验）。

输出仅三态：

```json
{
  "verdict": "confirmed|refuted|insufficient",
  "supportEvidence": ["ev_12"],
  "rejectedEvidence": ["ev_29"],
  "remainingUnknowns": [],
  "confidence": "high"
}
```

删除 Claim 仅当：`REFUTED` + ≥1 verified counter-evidence。

## 动态轮数与停止

```text
while claim.isImportant && claim.isDisputed
   && budget.available && evidenceStillExpandable:
     plan → acquire → review → recompute
```

停止：`CONFIRMED` / `REFUTED` / `EVIDENCE_CONVERGED` / `BUDGET_EXHAUSTED` / `UNRESOLVABLE`。
`EVIDENCE_CONVERGED`：嘴硬但无新证据、无有效反证 → 停。

## Budget

```text
priority ∝ importance × uncertainty × expected_information_gain
```

首版上限：`max_llm_rounds_per_claim=3`，`max_tool_escalation=3~5`，`max_test_attempts=2`。

## Final Synthesis

保留 Confirmed / Refuted / Unresolved + Evidence + Trace。
用户可见结构：Root Cause A（证据）/ Root Cause B（证据）/ Potential C（仍 unresolved 及原因）。
允许 unresolved，但必须是真没法验证。

## KPI（核心三个）

| Metric | 含义 |
|---|---|
| **Fusion Score** | 最终成绩 |
| **Oracle Capture Rate** | 吃掉多少 Oracle Union |
| **Fusion Regret** | 新制造多少错误 |

另：Best Fixed Single、Best Single Oracle、Oracle Union、Fusion Gain、Minority Truth Recovery、False Consensus Rate、Evidence Yield、Evidence Verification Rate、Cost/Correct Claim、Tokens/Gain。

比较基准是 **Best Fixed Single**（用户不能提前知道谁最好），同时追踪 Oracle Union。

## 回归集 `fusion-regression/`

必含：Q1 `defect-null-unwrap` / `defect-shared-map-race`；Q2 `root-shared-fixture-race` / `root-float-precision` / `blocking-ci`。
每条存：candidate outputs、claim graph、evidence、conflict、investigation、expected state、expected verdict。

三类必测：

1. **Minority Truth**（1:4、2:3）→ 调查，禁止 majority kill。
2. **独见真值**（1 家找到真缺陷）→ 保留并验证，禁止 1:4 删除。
3. **错误独见**（幻觉 race）→ 加法后调查，CodeGraph/test 证明后才 REFUTED。
   口号：**Add first, Verify later, Subtract only by proof.**

## 模块概念（不强制目录美学）

`orchestrator / claim-extractor / claim-normalizer / claim-store / conflict-detector / evidence-store / evidence-deduplicator / evidence-planner / investigation-router / providers/{search,codegraph,ast,lsp,git,test,runtime} / cross-reviewer / verifier / state-machine / synthesizer / budget-manager / metrics`

Provider 接口：

```ts
interface EvidenceProvider {
  canHandle(request: EvidenceRequest): Promise<CapabilityScore>
  investigate(request: EvidenceRequest, context: InvestigationContext): Promise<EvidenceResult>
}
```

## 实施顺序

| 阶段 | 内容 | 当前 |
|---|---|---|
| **P0** | Claim key/schema 全链路 canonical | 进行中（`claim_group_key` → `ClaimIdentity`） |
| **P1** | Claim Union + Claim Graph | 必做 |
| **P2** | Evidence Store + dedup | 必做 |
| **P3** | 新 Conflict 状态机 | 必做 |
| P4 | Evidence Planner | 变强阶段 |
| P5 | CodeGraph Provider | 变强阶段 |
| P6 | Cross Review | 变强阶段 |
| P7 | JEV evidence-only verifier | 变强阶段 |
| P8 | Test/Git/AST Provider | 扩展 |
| P9 | Budget + dynamic round | 扩展 |
| P10 | Fusion benchmark/metrics | 扩展 |
| P11 | UI Trace | 产品 |

P0–P3 不做完，CodeGraph 再强也是把证据倒进漏水的桶。

## 第一阶段硬验收

1. claim key 100% canonical
2. 所有模型 Claim 正确归并
3. minority claim 不因人数自动删除
4. disputed claim 自动创建 Investigation Plan
5. 无新 evidence 时自动升级 Provider
6. CodeGraph evidence 可进统一 Evidence Store
7. JEV 看不到 vote 数量
8. JEV 无 verified counter evidence 不得输出 REFUTED
9. Fusion trace 可完整重放
10. benchmark 同时输出 Best Single / Oracle Union / Fusion / Gain / Regret / Capture Rate

## 结论

实测已证明：**模型之间确实有互补信息；拉胯的是聚合和取证层，不是多模型 Fusion 方向本身。**
