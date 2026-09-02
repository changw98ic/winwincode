# ADR-0025：真实模型评估只控制交付阶段，不接管 Agent 执行

- 状态：已接受
- 日期：2026-08-23
- 对应任务：`winwincode-9c4.11.3`
- 运行器：[`scripts/live-evaluation.mjs`](../../scripts/live-evaluation.mjs)
- 命令入口：[`scripts/run-live-evaluation.mjs`](../../scripts/run-live-evaluation.mjs)
- 验收测试：[`tests/live-evaluation-runner.test.mjs`](../../tests/live-evaluation-runner.test.mjs)
- 操作说明：[`docs/live-evaluation.md`](../live-evaluation.md)
- 产品边界：[ADR-0023](0023-canonical-delivery-ownership.md)
- 确定性前置门禁：[ADR-0024](0024-deterministic-delivery-fixture.md)
- 可解释测量：[ADR-0026](0026-explainable-delivery-measures.md)

## 结论

真实模型评估必须由操作者显式加入，并在任何付费模型请求前通过无密钥的完整 Delivery 测试。每次运行固定仓库基线、Codex 与 DSH 源码、当前原生内核文件、DSH 提供商路由、模型、凭据引用、`DeliverySpec`、已批准方案、两次人工决定、预算、价格来源和投影版本。

运行器只按固定顺序推进 Delivery 的业务阶段：

```text
批准的 DeliverySpec
  ↓
Planner Codex Session
  ↓
人工方案批准
  ↓
Executor Codex Session
  ↓
冻结 Git Candidate
  ↓
独立 Reviewer Codex Session
  ↓
独立 Verifier Codex Session
  ↓
服务重新计算 Evidence、CriterionResult 和 Verdict
  ↓
人工交付批准
```

模型调用通过真实 DSH `llm-pi-ai` 提供商插件。配置只选择 DSH 已安装目录中的路由和模型；协议、上下文、输入能力、推理强度和消息兼容规则继续由 DSH 解析。可选 `baseURL` 只覆盖目录端点，不会把模型重新声明成另一条路由。工具、文件写入、测试、Plan 和子 Agent 全部由同一个内嵌 Codex Core 决定和执行。运行器不创建 WinWinCode Agent 列表、任务图、mailbox、工具循环或第二套调度状态。

## 候选冻结边界

Executor 在 Codex 的工作区写入权限内修改源码和运行检查，但不修改 `.git`。执行轮结束后，阶段控制器把当时的全部 Git 可见改动一次性加入索引，并以配置中固定的提交说明建立候选提交；提交钩子和签名在这份隔离副本中关闭。阶段控制器不会生成、改写或挑选源码内容。

提交冻结后，运行器从该 commit 再建立一份干净的只读审核副本。Executor 留下的忽略文件、依赖缓存或生成物不会进入 Reviewer/Verifier 的工作目录；验证证据只能来自冻结 Git tree 中的内容和审核阶段自己允许的只读检查。

这样处理有三个直接原因：

1. Codex 工作区沙箱会保护 `.git` 元数据，模型不需要得到额外 Git 历史写权限；
2. Reviewer 与 Verifier 得到一个确定的 commit、tree、Diff 摘要和变化路径集合，不会审核持续变化的工作区。
3. Executor 工作区中的忽略文件不会被误当成候选内容或验证依据。

如果 Executor 改写了 Git 历史、没有产生 Git 可见改动，或冻结后工作区仍不干净，运行立即失败。Reviewer 与 Verifier 使用独立只读 Session，并且只能引用各自运行记录中真实存在的规范化事件 ID。

Reviewer 与 Verifier 的最终结果属于 StrongFlow 验收协议。每个角色先提交一次结构化结果；格式或协议校验未通过时，同一只读 Session 最多获得一次纠正机会。两次事件都会留在追加式运行台账中，投影只从最新轮次生成 finding 与 evidence，上一轮结果不会参与 Verdict。运行台账投影把可引用的 `citation` 与观察到的 `outcome` 分开；`citation` 中的 `type` 与 `event_id` 是不可重新解释的组合，Verifier 只复制该对象，不能把 `outcome` 填入结果，也不能把执行测试的 `command` 自行改写为 `test`。第二次仍未形成完整结果时，运行在验收阶段结束并保持未交付状态。

角色轮次是否完成以追加式台账中的终止事件为准。轮次已经完整结束后，DSH 若已先行移除空闲 Session，本地句柄释放返回 `SESSION_NOT_FOUND` 只表示清理已经完成，不会推翻现有验证事实；其他清理错误仍然中止运行。

## 预算

配置必须同时给出：

- 最长运行时间；
- 最大模型调用次数；
- 正常流程和有上限结果纠正允许的最大轮数；
- 单次输出 token 上限；
- 总 token 上限；
- 总费用上限；
- 带来源说明的输入、输出和缓存价格。

时间、轮数、调用次数和单次输出上限会在开始下一项操作前检查。总 token 与总费用只能在提供商返回用量后计算，因此最多可能超过一个已经受单次上限约束的模型调用；发现后不会再开始下一次调用。结果保留逐次用量、估算费用、价格来源和触发的限制。

## 结果与凭据

每次运行独占 `OUTPUT/RUN_ID/`，并持续原子更新权限为 `0600` 的 `result.json`。结果包括：

- 精确的 Codex、DSH 和原生文件身份；
- 规范化后的完整 `DeliverySpec`、方案、人工决定脚本、执行输入及各自摘要；
- DSH 目录、路由、模型、解析后的安全能力摘要、可选端点覆盖与凭据环境变量名称；
- 仓库基线、隔离工作区和候选身份；
- Delivery、阶段、角色及 DSH/Codex Session 绑定；
- 安全的 Plan、Agent、工具、Diff、用量与 Evidence 投影；
- Attention、逐项验收结果和最终 Verdict；
- 带来源的完整度、可信度、稳定性、人工依赖度和效率测量；
- 当前阶段、阶段历史、预算和结束原因类别。

结果不复制原始聊天、命令输出或提供商错误正文。配置只接受名称含 `KEY`、`SECRET` 或 `TOKEN` 的凭据环境变量引用；DSH 从自己的启动环境读取该值，Codex 默认 Shell 环境会排除它。环境中的凭据值还会在每次结果写入前再次检查和清除。失败、预算终止与信号中断都会保留最后一个已确认阶段和安全结果路径。

## 后果

- 这项评估可以比较真实模型和真实仓库上的交付表现，同时继续使用生产 DSH、Codex 与 StrongFlow 边界。
- 同一个 `runId` 不能覆盖旧结果；重新运行必须使用新的身份。
- 评估结果是派生的测量文件，不写入十项 Delivery 业务对象，也不成为新的执行权威。
- 真正的模型效果会受到仓库、模型版本、提供商行为和操作者固定输入影响；后续评分只能从这些可追溯事实派生。
