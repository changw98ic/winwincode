# ADR-0038：Fusion × JEV 联合验证与调优

- 状态：已接受
- 日期：2026-09-22
- 对应任务：`winwincode-community.5`、`winwincode-community.4`
- 记忆键：`fusion-jev-joint-bench-20260922`
- 关联：[ADR-0037](0037-agent-fusion-engine.md)、[ADR-0036](0036-multi-round-fusion-convergence.md)

## 目标

同时回答：

1. Fusion 是否真的比单 Agent / 单模型更强  
2. Fusion 的增益来自哪里  
3. JEV 是否在降低上下文成本的同时保持任务状态完整  
4. Fusion + JEV 组合是否仍产生净收益  

优化目标（不只 accuracy）：

```text
Quality + Evidence Coverage + Context Integrity + Cost + Latency + Stability
```

```text
Fusion Gain > Fusion Regret
JEV Context Loss ≈ 0
Fusion+JEV：质量基本不降，显著降低 token / 上下文压力
```

## 设计原则

**Fusion**（同 ADR-0037）：默认加法；争议→调查；无新证据→升级；仅 verified counter 可减；Consensus 是 metadata。

**JEV**：

```text
Canonical truth 不允许因 rebuild 丢失
Working history 可以 aggressively compress
JEV 管上下文，不修改业务真值
失效信息必须退出 Active Context
```

## 三层上下文

| 层 | 谁写 | 内容 |
|---|---|---|
| **L1 Canonical State** | Fusion / Verifier（JEV 不得改语义） | objective、acceptance、constraints、confirmed/refuted/disputed claims、verified evidence、hypotheses、decisions、tool/test facts |
| **L2 Active Context** | JEV 动态构造（Agent 实际所见） | 当前目标/阶段/高优 claim/相关 evidence/必要代码/限制/最近关键 observation |
| **L3 Archive** | JEV | 旧 reasoning、raw tool、重复搜索、失败路径、过期 hypothesis、大日志 |

## 四组基线（每批任务）

| 模式 | Fusion | JEV | 回答 |
|---|---|---|---|
| A | OFF | OFF | 单 Agent 基础能力 |
| B | OFF | ON | JEV 是否省 token / 是否破坏单 Agent |
| C | ON | OFF | Fusion 质量上限、Oracle Capture、Gain、Regret |
| D | ON | ON | 产品形态：`D vs C` |

理想：`Quality(D)≈Quality(C)` 且 `Token(D)<<Token(C)`；更理想 `Quality(D)>Quality(C)`。

## 任务来源与类别

真实历史 bug / PR / issue / 开源 fix / mutation / CI 故障 / 并发 / 数据流 / 跨模块等。  
每类 20–50 cases（调试期 5–10）。**正式结果禁止 2 题宣布胜利。**

## 自动分类样本

| 类 | 测什么 |
|---|---|
| Consensus Correct | Fusion/JEV 不制造 regression |
| **Complementary Truth** | Fusion 是否超过固定单模型（最重要） |
| Minority Truth | 是否保留少数真值 |
| **False Consensus** | 工具是否突破 Model Oracle |
| False Minority | 能否 evidence refute 幻觉 |
| Evidence Conflict | 多轮升级 |

## Fusion 指标

- Fusion Score、**Best Fixed Single**、Best Single Oracle、Oracle Union  
- **Oracle Capture Rate**（→100%）  
- **Fusion Gain**、**Fusion Regret**（→0）  
- Minority Truth Recovery、**False Consensus Recovery**（证明 Ceiling > Model Oracle）  
- Investigation Gain  

## JEV 指标

- Constraint Recall = 100%  
- Claim Recall（Confirmed/Refuted/Disputed 不得混）  
- Evidence Recall ≥99%、**Evidence Binding Accuracy = 0 error**  
- **State Regression Rate = 0**（Confirmed 不得回退，除非新 verified counter）  
- **Stale Fact Rate ≈ 0**（Refuted 不得回 Active Hypothesis）  
- **Hallucinated State Rate = 0**  
- Compression（最后看；禁止「压 90% 掉 15% accuracy 还宣布成功」）  
- **Post-Rebuild Task Success**（rebuild 后继续干活，不只背约束）  

## Rebuild 实验

- 深度：0/1/3/5/10 rebuild → Rebuild Degradation Curve  
- 阈值：20%–80% 或 16k–96k tokens  
- 动态分（后续）：

```text
rebuild_score = context_pressure + stale_ratio + duplication_ratio
              + tool_noise + inactive_history_ratio
              - active_evidence_density - current_reasoning_dependency
```

## JEV 压缩边界

**Pinned**：objective、acceptance、hard constraints、current plan、confirmed/refuted claims、verified evidence、unresolved high-risk claims、executor instruction、verifier requirements。

**优先压缩**：重复 reasoning、旧 tool output、已总结代码、失败探索、过期 hypothesis、低价值日志、重复文件、无关 turn。

## Fusion × JEV 关键长链路

```text
Fusion R1 → 6 claims → ReAct 10 calls → JEV rebuild#1
→ Fusion checkpoint → CodeGraph → new evidence → ReAct 10
→ JEV rebuild#2 → claim 状态变化 → checkpoint → test
→ JEV rebuild#3 → Verifier
```

检查：最初 claim 还在、新 claim 加入、refuted 退出 Active、evidence 绑定正确、Fusion 指令完整、结果正确。

## 消融

**Fusion**：F0 single → F1 union → F2 +conflict → F3 +CodeGraph → F4 +cross review → F5 +JEV verifier → F6 +dynamic routing  

**JEV**：J0 none → J1 truncate → J2 summary → J3 pin+summary → J4 pin+drop+archive → J5 full rebuild  

**联合矩阵（代表点，不全排列）**：F0J0、F0J5、F3J0、F3J5、F6J0、F6J5。

## 成本 / Cache

记录 input/output/cached/rebuild/tool/Fusion tokens、LLM/tool cost、latency。  
派生：Cost per Correct Claim / Resolved Conflict / Fusion Gain；Tokens per Fusion Gain。  
Cache：hit/miss、rebuildFrequency、prefixReuse → **Net Token Saving**（不是表面 compression）。

## 存储结构

```text
benchmark_run / task_run / agent_run / fusion_round
claim / claim_state_transition / evidence / investigation
tool_call / jev_rebuild / context_snapshot / verification / metric
```

`context_snapshot`：每次 JEV 前后 before/after state。

## Rebuild Gate（确定性，不靠 LLM）

```text
build candidate → integrity check → PASS: activate
                                  → FAIL: discard → fallback previous
```

检查：pinned constraints 在？Confirmed claims 在？Verified evidence 引用有效？无 Refuted→Supported 回退？无不存在 evidence ID？

## Fusion Verification Gate

`Claim → Confirmed` 至少：model consensus **或** verified evidence **或** verifier。  
**高风险 claim 必须 evidence / verifier，单纯 consensus 不够。**

## 分阶段调优

1. **Phase 1 Instrumentation**：claim canonical、evidence ID、state log、context snapshot、JEV rebuild log、fusion round log、agent profile、token/cost。  
2. **Phase 2 Baseline**：四组 A–D，30–50 任务。  
3. **Phase 3 Fusion 调优**（固定 JEV）。  
4. **Phase 4 JEV 调优**（固定 Fusion）。  
5. **Phase 5 联合优化**。  
禁止第一版上 RL 黑盒策略。

## 第一阶段验收门槛

```text
Fusion Regret <= 1%
Minority Truth Recovery >= 95%
Oracle Capture >= 95%
Constraint Recall = 100%
Confirmed Claim Recall = 100%
Verified Evidence Recall >= 99%
Evidence Binding Error = 0
State Regression = 0
Hallucinated State = 0
```

JEV 成本目标待真实数据后取 Pareto Front（Quality × Cost × Latency）。

## 最终闭环

```text
Agent 执行 → Evidence → Fusion 更新 Canonical State
→ JEV 构建 Active Context → Agent 继续 → … → Verifier
```

> Fusion 尽可能提高每一单位计算产生的「正确新知识」；JEV 尽可能减少维持这些知识所需的上下文成本，同时绝不能破坏已经确认的任务状态。
