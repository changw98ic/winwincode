# ADR-0035：JEV 作为强裁判与验证调度，不覆盖更硬证据

- 状态：已接受
- 日期：2026-09-22
- 对应任务：`winwincode-community.5`（Fusion 聚合）、`winwincode-community.5.4`（Evidence Adjudicator）、`winwincode-community.5.7`（Fusion × Jev）
- 记忆键：`fusion-jev-adjudication-20260922`
- 关联实测：`fusion-live-panel-20260922`（Q2 同级冲突 `leading=None` 丢 claim）
- 相关：[ADR-0026](0026-explainable-delivery-measures.md)、`winwincode-community.5` Fusion 第一批

## 结论

JEV 不是“最高法院”，而是**强裁判 + 验证调度器**。完全拿掉 JEV 会丢掉一类关键能力：架构判断、根因归因、方案合理性、潜在回归等争议，很多无法立刻用 test / log / tool 直接证明。工具证据也有覆盖率上限。

JEV 可以提升或降低某个 claim 的可信度，**不得仅凭语言判断覆盖已经存在的更硬证据**。真实测试一旦证明 JEV 判断错误，JEV 必须让位。

## 证据优先级

```text
真实验证事实
  >
工具直接证据
  >
JEV 基于完整材料的裁判结果
  >
模型共识
  >
单模型主张
```

与现有 `DecisionAuthority`（Verified Fact > Tool Observation > Model Consensus > Single Model Claim）对齐时，在 Tool Observation 与 Model Consensus 之间插入 **JevAdjudication** 层；其权重高于同级模型投票，低于工具与验证事实。

## 硬限制

JEV 的输出是 `provisional`（临时采信），不是 ground truth。

```text
JEV confidence 高 + 没有更强反证 → provisional leading
JEV confidence 低               → disputed（双方保留）
Verifier 后续验证               → confirmed / rejected
```

## JEV 的三个位置

1. **语义裁判**：判断两个 claim 是冲突、包含，还是同一件事的不同说法。用于缓解 `ClaimMismatch`（claimKey+summary 措辞漂移导致对齐失败）。
2. **争议裁判**：多模型证据都只是“读代码 + 推理”、工具暂无法直接验证时，由 JEV 给出 `leading`。此时不再因为 2:1 且同级证据就机械 `leading=None`。
3. **验证规划**：不只回答“谁更可能对”，还回答“要证明 A/B 谁对，需要执行什么检查”。

## 裁决输出

```json
{
  "conflict": {
    "claim_a": "...",
    "claim_b": "..."
  },
  "judge": {
    "leading": "claim_a",
    "confidence": 0.81,
    "reason": "...",
    "evidence_refs": ["..."]
  },
  "verification": {
    "verifiable": true,
    "actions": [
      "inspect transaction lifecycle",
      "run concurrency test",
      "check pool metrics"
    ]
  }
}
```

示例（根因争议）：

```text
GLM:     根因是 transaction 未释放
MiMo:    根因是 pool 太小
DeepSeek: transaction 未释放

JEV:
- transaction 未释放：更符合日志与代码路径
- pool 太小：属于放大因素，不是根因
confidence = 0.82

→ 生成验证建议
→ 检查 connection lifecycle / transaction duration / 并发测试
→ Verifier
→ 以真实结果为准
```

## 同级冲突处理（相对实测的必改点）

实测原路径：

```text
同级冲突 → leading=None → 映射丢弃 → Fusion 变差
```

改为：

```text
同级冲突
  → JEV 语义/争议裁判
      → 能明显区分 → provisional leading（双方与理由保留）
      → 无法判断   → disputed（双方保留，不丢弃）
  → 存在可执行验证 → Verifier
  → 最终 Fusion 以验证事实为准
```

`map_fusion_analysis_to_fixture` 不得把 `leading=None` 的争议 claim 静默丢掉；必须保留为 `disputed` / `undecided` 可见状态。

## 完整链路

```text
多模型独立生成
      ↓
Claim 对齐（claimKey+summary 规范化）
      ↓
共识 / 冲突 / 独见
      ↓
JEV 语义裁判
      ↓
Evidence / Verification Planner
      ↓
Tool / Test / Verifier
      ↓
最终 Fusion
```

## JEV 不可用时的降级

```text
JEV available:
    conflict → JEV judge → verifier if needed

JEV unavailable:
    conflict → retain anchor + disputed
             → verifier if possible
```

**禁止** JEV 一挂就退化成“冲突 = 删除”。

## 与 Model Dynasty 分层的关系

```text
Deterministic → Jev → Single Model → Fusion
```

JEV 仍是高频、廉价、闭集判断层；Fusion 仍是低频高价值聚合。本 ADR 只调整 JEV 在 Fusion 裁决中的**权限边界**，不把 JEV opinion 等价于 ground truth，也不让 Fusion / JEV 覆盖 Verifier。

## 明确不做

- 不把 JEV 裁决结果写成不可推翻的 Canonical 结论
- 不因 JEV confidence 高而跳过可执行验证
- 不在 JEV 故障时丢弃冲突 claim
- 不把模型多数票直接当真相

## 验收

1. 同级 2:1 冲突在 JEV 可区分时产生 `provisional leading`，且双方 claim 与理由仍可审计。
2. JEV 不可区分时双方保留为 `disputed`，映射到 fixture 不丢信息。
3. JEV 挂掉时冲突仍保留 anchor + disputed，可走 Verifier 则走。
4. Verifier 真实结果可 `confirmed` / `rejected` 推翻 provisional leading。
5. `ClaimMismatch` 由语义对齐吸收措辞漂移，或降级为显式 unmatched，不得静默丢单。
