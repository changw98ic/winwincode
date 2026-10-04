# ADR-0038：Fusion × JEV 联合验证与调优

- 状态：已接受
- 日期：2026-09-22
- 对应任务：`winwincode-community.5`、`winwincode-community.4`
- 记忆键：`fusion-jev-joint-bench-20260922`
- 关联：[ADR-0037](0037-agent-fusion-engine.md)、[ADR-0036](0036-multi-round-fusion-convergence.md)

> 2026-09-24 修订：正式评测固定为 20 个 GitHub 公开任务、A/B/C/D 四组、
> 四个单模型与独立 `fusion(4)` 各运行一次；全部模型调用使用 max 思考强度，
> 单任务和全批次都不设资源或费用上限。修订后的
> 完整实施合同见 [ADR-0039](0039-integration-fusion-snapshot-remediation.md)。

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

## 四组主实验与五个对照 run

| 组 | 后置 Fusion 处理 | JEV | 回答 |
|---|---|---|---|
| A | OFF | OFF | 单 Agent 基础能力 |
| B | OFF | ON | JEV 是否省 token / 是否破坏单 Agent |
| C | ON | OFF | Fusion 质量上限、Oracle Capture、Gain、Regret |
| D | ON | ON | 产品形态：`D vs C` |

每组固定产生以下五个对照结果，每个任务、每格恰好一次：

| 对照 ID | 组成 |
|---|---|
| `glm5.1flash` | 单模型 |
| `mimov2.6pro` | 单模型 |
| `ds4.1flash` | 单模型 |
| `qwen3.8flash` | 单模型 |
| `fusion(4)` | 独立调用上述四模型各一次，并做一次聚合 |

`fusion(4)` 不是第五个 provider，也不是另一层候选后处理。它是一个独立的
四模型聚合 run：四个成员各自产生一份结果，再恰好聚合一次；不复用四个单模型
对照 run 的输出，也不对聚合结果递归融合。五个对照 run 在 A/B/C/D 中均不得
省略。正式主矩阵包含 `20 × 4 × 4 = 320` 个单模型 run 和
`20 × 4 × 1 = 80` 个 `fusion(4)` run，共 400 个评测 run。每个 Fusion run
内部的四个成员调用和一次聚合调用必须分别计量。

理想：`Quality(D)≈Quality(C)` 且 `Token(D)<<Token(C)`；更理想 `Quality(D)>Quality(C)`。

## 任务来源与类别

正式任务固定为 20 个，来源为公开题库
<https://github.com/changw9813/agent-benchmark-tasks>，当前冻结 revision 为
`fa9da301e493fb88d48c86cb8954ed46d9cd2ffe`。任务覆盖多语言工程实现、
状态机、解析、并发模拟和数据处理，并绑定公开需求与私有独立验收。

提交仓库为独立公开仓库
<https://github.com/changw98ic/agent-benchmark-submissions>。题库保持只读，
受测 Agent 不直接推送题库；调度器把冻结结果按 `submit/<task-id>/<run-id>`
写入提交仓库。旧评测记录、旧运行结果和旧 Snapshot 事实不进入新结果集，
也不迁移到新实验或提交仓库。

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

**JEV**：Context-only、Judge-only、Full JEV。三种消融均交叉四个单模型 run
和一个独立 `fusion(4)` run，每个任务、每格恰好一次，共 300 个消融 run
（240 个单模型 + 60 个 Fusion）；不得用 Full JEV 的总体收益替代两个子功能
的独立结果。

主实验与消融矩阵均为冻结的全排列，不采用代表点抽样。

## 成本 / Cache

记录 input/output/cached/rebuild/tool/Fusion tokens、LLM/tool cost、latency。
派生：Cost per Correct Claim / Resolved Conflict / Fusion Gain；Tokens per Fusion Gain。
Cache：hit/miss、rebuildFrequency、prefixReuse → **Net Token Saving**（不是表面 compression）。
单任务和全批次都不设 token、调用、墙钟或费用上限；成本照实计量，不把无上限
当作零成本。

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

`Claim → Confirmed` 必须有可解析 Source Receipt 和 Claim Verification。
model consensus 只是 metadata，不能确认事实。反驳已有支持的 claim 必须有
同命题、同范围、同适用版本的 verified counter。

## 分阶段调优

1. **Phase 1 Instrumentation**：claim canonical、evidence ID、state log、context snapshot、JEV rebuild log、fusion round log、agent profile、token/cost。
2. **Phase 2 Baseline**：20 个公开任务，四组 A–D × 四个单模型与独立
   `fusion(4)`，每格一次。
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

## 卡死停止

单任务和全批次都不设时间和资源上限。工具请求身份由工具名、目标文件或资源、
规范化参数和请求内容摘要组成，排除请求 ID、时间戳及进度元数据。同一身份在
一个 run 中累计出现 6 次，即重复请求相同内容超过 5 次；第 6 次请求必须在
实际执行前被拦截，立即记录 `STUCK_TOOL_REPEAT_LIMIT` 并停止当前任务。
Runner 继续领取其他任务；当前任务的失败保留在分母中。Fusion 任务停止后不再
启动该任务的其他成员或聚合调用。账本或必要证据无法可靠保存时停止整批，
恢复时复用已领取任务的原调用身份，已完成或已停止任务不重复执行。

共用 Server 与 Device 的任务通过入口 ProductSession 绑定执行范围。Controller
为 WorkRun 创建的角色 ProductSession 按实际 launch grant 归入该任务；停止检查
同时核对这些角色身份和物理 Worker 中的 Core run。已停止任务的事实继续保留，
其他任务仍可启动并完成自己的执行、候选与验收流程。

正式评测 Runner 固定为本机 `macOS 26.5.1 aarch64-apple-darwin`，不需要另行
选择或确认 Runner。

## 最终闭环

```text
Agent 执行 → Evidence → Fusion 更新 Canonical State
→ JEV 构建 Active Context → Agent 继续 → … → Verifier
```

> Fusion 尽可能提高每一单位计算产生的「正确新知识」；JEV 尽可能减少维持这些知识所需的上下文成本，同时绝不能破坏已经确认的任务状态。
