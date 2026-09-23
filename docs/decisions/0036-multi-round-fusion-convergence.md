# ADR-0036：Multi-round Fusion 是证据收敛，不是一次性投票

- 状态：已接受
- 日期：2026-09-22
- 对应任务：`winwincode-community.5.13`、`winwincode-community.5`（Fusion）
- 记忆键：`fusion-multiround-convergence-20260922`
- 关联：[ADR-0035](0035-jev-strong-judge-verification-scheduler.md)、实测 `fusion-live-retest-20260922`（Q2 `claim:root-shared-fixture-race` 2:3 被 JEV 直接裁错）

## 一句话定义

> Multi-round Fusion = 多模型独立探索形成知识并集，再围绕争议点进行定向多轮取证，通过**新增证据**逐步收敛，而不是一次投票后选择某一方。

## 硬规则（状态机）

```text
Round1 独立探索出现同级争议（例如 2 支持 / 3 反对）
  → 必须进入 Round2 定向取证
  → 禁止用 JEV / 多数票直接终裁
```

Q2 `root-shared-fixture-race` 是回归样例：

```text
正确：→ Round2 targeted investigation
错误：→ JEV 直接裁决
```

一轮结果只叫「收集意见」，不叫完整 Fusion。所谓完整 Fusion 至少是：

```text
Discovery → Union → Conflict Detection → Targeted Investigation → Verification → Synthesis
```

## 阶段与目标（每轮目标不得相同）

| Round | 目标 | 产物 |
|---|---|---|
| R1 Discovery | 最大化发现（claim / root cause / evidence / edge case coverage），互相不可见 | **Union**（先做加法） |
| R2 Conflict Detection | 结构化分类，不删 claim | CONSENSUS / DISPUTED / UNIQUE / UNVERIFIED / CONTRADICTED |
| R3 Targeted Investigation | 只打争议点；必须**新证据** | new evidence / reproduction / code path / counterexample |
| R4 Verification | JEV / Verifier 看证据增量，不看票数 | CONFIRMED / REFUTED / UNRESOLVED |
| R5 Synthesis | 最终综合 | verified(K) |

禁止：

```text
R1 回答问题
R2 再回答一遍
R3 再回答一遍
```

那是抽卡，不是 Fusion。

## Round 3 题面约束

不要整题重发。只发争议包：

```text
claim: root-shared-fixture-race
支持方证据: E1 E2
反对方证据: E3 E4
任务:
1. 检查 E1–E4
2. 找出具体错误假设
3. 提供新的代码证据
4. 不允许仅重复原结论
```

**没有增量就不给权重。** 必须出现下列之一：

```text
new evidence
new reproduction
new code path
new counterexample
```

否则只是「我还是觉得不是」的家庭群争论。

## Round 4：JEV / Verifier 看什么

错误输入：

```text
3 家反对 / 2 家支持
```

正确输入：

```text
Round1 evidence
+ Round2 新增 evidence
+ counterexample
+ reproduction result
```

JEV 输出只允许：

```text
CONFIRMED   已被新证据确认
REFUTED     已被明确证伪
UNRESOLVED  仍未解决（禁止硬猜 TRUE/FALSE）
```

允许的成熟表述：

> 目前证据不足以排除 A，但尚不能确认为主要根因。

## 知识单调性

```text
K1 = 第一轮知识
K2 = K1 + 冲突分析
K3 = K2 + 新证据
K4 = K3 − 已被明确证伪内容
Final = verified(K4)
```

要求：

```text
Round N knowledge ≥ Round N-1 knowledge
```

唯一允许做减法的地方是**明确证伪**，不是「别人不同意」。

## 终止条件（四种，任一满足即停）

1. **Consensus convergence**：无关键争议。
2. **Evidence convergence**：仍有口头反对，但已无新反证。`disagreement != useful disagreement`。
3. **Budget limit**：`max_rounds` / `max_extra_tokens` / `max_verification_cost`。
4. **Unresolvable**：`claim = disputed`，不强制 TRUE/FALSE。

## 相对 ADR-0035 的收紧

ADR-0035 允许 JEV 对 evidence-tied 冲突给 `provisional leading`。本 ADR 收紧为：

| 时机 | 是否允许 JEV provisional |
|---|---|
| Round1 刚出现争议、尚无 R3 新证据 | **禁止** |
| R3 已产生新证据 / 反例 / 复现 | 允许，且必须引用增量证据 |
| 已有工具/验证事实更硬 | 仍禁止覆盖（ADR-0035 不变） |

Verifier 仍不可被覆盖。

## 实现落点

- `fusion_analysis`：`FusionRound` / `FusionNextAction` 状态；争议在 R1 后不得直接 `JevProvisional`。
- `fusion-live-panel` harness：R2 分类 → R3 定向取证包 → R4 验证。
- 计分：UNRESOLVED 记入 `disputed_keys`，不记 missing，不记 wrong。

## 验收

1. 同级争议（含 2:3）在无 R3 新证据时，状态机返回 `TargetedInvestigation`，且 `apply_jev_conflict_leading` 拒绝或仅在 `post_investigation=true` 时生效。
2. R3 响应若无新证据/复现/反例/代码路径，不得提高该侧权重。
3. R4 只基于 R1+R3 证据包输出 CONFIRMED / REFUTED / UNRESOLVED。
4. UNRESOLVED claim 在报告与计分中保留为 `disputed`。
5. 四种终止条件均可触发且可审计。
