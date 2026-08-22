# ADR-0016：角色交接由程序选择、固定并从本地记录重建

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.6.3`、`winwincode-9c4.6.4`
- 实现：[`packages/strongflow/src/handoff.ts`](../../packages/strongflow/src/handoff.ts)
- 格式：[`packages/contracts/src/strongflow-handoff.ts`](../../packages/contracts/src/strongflow-handoff.ts)
- 测试：[`tests/strongflow-handoff.test.mjs`](../../tests/strongflow-handoff.test.mjs)

## 结论

StrongFlow 不把共享聊天记录交给后续角色。`StrongFlowHandoffBuilder` 同时读取作业事件和制品存储，按当前流程状态选择角色真正需要的正式制品，再生成一份只追加的 `StrongFlowHandoffManifest`。

每份交接记录：

- 当前作业序号和目标人工审核或角色；
- 角色的阶段运行 ID 与尝试 ID；
- 当前定义和适用时的完整冻结候选版本；
- 每项输入的固定顺序、制品种类、制品 ID、存储记录 ID、正文摘要和字节数；
- 输入总字节数、限制值、程序生产者和时间。

交接正文也写入内容寻址存储。交接 ID 由上述固定内容的 SHA-256 计算。相同阶段重试读取会得到同一交接；用相同 ID 提交不同内容会失败。

## 各角色看到的内容

- Requirements Analyst：只看到一份 `UserRequest`，不会收到 `SolutionDesign` 或共享对话；代码事实通过只读工作区工具核对后写入 `RequirementSpec`。
- Solution Architect：只看到当前 `RequirementSpec`。
- 人工审核：看到当前 `RequirementSpec`、`SolutionDesign` 和两张由程序确定生成的定义图。
- Planner：只看到上述定义集和与它完全一致的已批准 `HumanReviewRecord`。
- Executor：只看到由当前批准定义产生的 `ExecutionPlan`。计划对定义和人工审核的来源引用由程序验证；执行制品仍保留完整传递来源，不把额外定义正文放进执行模型上下文。
- Reviewer：看到批准定义、执行计划和当前冻结候选版本的 `PatchManifest`。
- Verifier：在 Reviewer 输入之上看到当前 `ReviewReport`。
- Adversarial Verifier：再看到同一次验证阶段产生的标准 `VerificationReport`。
- Remediator：看到同一冻结候选版本的批准链、报告和由程序生成的 `RemediationRequest`。请求中的每项修复指令必须对应实际开放的审查问题或失败的验证检查。

角色输入顺序来自唯一的角色策略，调用方不能增加共享对话、跳过人工审核或换入任意制品。

## 当前状态和旧内容

方案、图和人工审核必须等于作业投影里的当前定义。Planner 和 Executor 在没有有效批准时不会得到交接。执行计划必须来自作业事件里成功的 Planning 阶段运行；代码变更必须来自成功的 Execution 或 Remediation 阶段运行；审查和验证报告必须来自当前验证阶段的同一运行与尝试。

如果成功事件记录了内核会话 ID，制品记录里的内核事件范围还必须属于同一个会话。另一个会话即使复用了相同阶段运行和尝试 ID，也不会被选中。后来追加的其他计划或报告不会改变已经由成功事件选定的输入。

候选版本阶段会逐项比较完整候选身份，包括来源快照、基础提交与树、候选提交与树以及差异摘要。任何一个报告、请求或变更清单指向另一个候选版本时，交接在模型运行前失败。

## 重启重建

`reconstruct` 只按已发布交接中的记录 ID 读取输入，不重新执行“找最新制品”的选择。它再次核对每项制品身份、正文摘要、字节数、输入顺序、目标角色和交接内容摘要。流程已经继续、作业已经重启或后来出现其他合法制品时，原交接仍会重建成相同的模型输入。

## 上下文限制

交接默认最多引用 8 MiB 的正式制品正文，产品上限是 64 MiB。超过限制时不会发布交接。角色运行器还会计算包含固定系统说明、角色身份和输入包装在内的最终提示字节数；最终提示超过配置上限时，不创建事件迭代器，也不向内核提交回合。

## 完整性和重放验证

以下检查使用真实本地存储和重启后的新实例，不用伪造的存储替身：

| 攻击或故障 | 预期结果 | 回归测试 |
| --- | --- | --- |
| 修改需求正文、删除正文或改写元数据 | 摘要、大小或记录链核对失败 | `strongflow-artifact-store.test.mjs` |
| 把方案输入换到需求位置，或替换定义图身份 | 交接输入顺序或定义关系核对失败 | `strongflow-handoff.test.mjs` |
| 使用旧图提交人工批准 | 人工审核返回 `STALE_DEFINITION`，流程继续等待新审核 | `human-review-gate.test.mjs`、`strongflow-recovery.test.mjs` |
| 使用另一个内核会话产生的同阶段制品 | 只按成功事件记录的会话选择；没有唯一匹配就失败 | `strongflow-handoff.test.mjs` |
| 同一成功尝试在首次交接前产生两份结果 | 返回 `ARTIFACT_AMBIGUOUS`，不任选一份 | `strongflow-handoff.test.mjs` |
| 已固定交接后又追加另一份合法结果 | 重启后仍读取原记录 ID 和原正文摘要 | `strongflow-handoff.test.mjs` |
| 报告换入另一个冻结候选版本 | 返回 `STALE_CANDIDATE`，不会进入下一个角色 | `strongflow-handoff.test.mjs` |
| 使用不支持的制品或交接格式版本 | 在边界解析时明确拒绝 | `strongflow-artifact.test.mjs`、`strongflow-handoff.test.mjs` |
| 直接命令证据正文丢失 | 返回 `CONTENT_MISSING`，错误信息不复制证据正文 | `strongflow-artifact-store.test.mjs` |
| 发布中断，只留下 `.pending-*` 文件 | 忽略未发布文件；已公开但不完整的记录视为损坏 | `strongflow-artifact-store.test.mjs` |
| 重启后重建人工、计划、执行、审查、验证和修复交接 | 每份交接的清单和模型输入逐项完全相同 | `strongflow-handoff.test.mjs` |

错误对外只提供稳定错误代码和发生核对的位置，不把需求正文、模型输出、命令证据或凭据复制进错误消息。详细原始内容仍留在受作业边界约束的本地记录中。
