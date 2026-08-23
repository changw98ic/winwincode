# ADR-0026：交付评估保留五组可解释事实，不生成黑盒总分

- 状态：已接受
- 日期：2026-08-23
- 对应任务：`winwincode-9c4.11.4`
- 生产实现：[`packages/strongflow/src/evaluation-measures.ts`](../../packages/strongflow/src/evaluation-measures.ts)
- 真实结果适配：[`scripts/evaluation-measures.mjs`](../../scripts/evaluation-measures.mjs)
- 重算命令：[`scripts/run-evaluation-measures.mjs`](../../scripts/run-evaluation-measures.mjs)
- 验收测试：[`tests/evaluation-measures.test.mjs`](../../tests/evaluation-measures.test.mjs)
- 真实运行测试：[`tests/live-evaluation-runner.test.mjs`](../../tests/live-evaluation-runner.test.mjs)
- 产品边界：[ADR-0023](0023-canonical-delivery-ownership.md)
- 真实评估：[ADR-0025](0025-live-delivery-evaluation.md)

## 结论

WinWinCode 从已有 `Delivery`、运行投影、模型调用记录和评估时间派生一份只读测量结果。它回答五组具体问题：

1. **交付完整度**：每项验收条件是通过、失败、无法判断、基础设施错误，还是缺少结果；必需条件通过比例是多少。
2. **结果可信度**：当前候选引用了多少直接证据和评审发现；是否缺少证据引用；必需的 Reviewer 与 Verifier 是否结束，并且是否都留下当前候选的发现。
3. **执行稳定性**：出现过多少阶段失败、取消、返工、任务失败、超时、权限拒绝、基础设施失败、运行错误、恢复、Agent 失败或中断。
4. **人工依赖度**：有多少人工阶段、业务 Attention、执行审批和用户输入；哪些已经处理，哪些仍在阻塞。
5. **执行效率**：实际经过时间、各阶段耗时之和、模型调用、token、估算费用、Agent 数量，以及从 Agent 生命周期事件观察到的最大并行数。

这五组结果保持分开。实现没有 `overallScore`、加权总分或由模型主观给出的分数。后续报表可以显示这些事实，但不能用一个数字遮住失败原因、证据缺口或人工阻塞。

## 来源绑定

每一个数字、状态和布尔判断都使用同一形状：

```ts
interface SourcedMeasure<Value> {
  value: Value
  sourceRefs: DeliveryMeasureSourceRef[]
}
```

来源指向具体的评估运行、Delivery revision、验收条件、逐项结果、Verdict、Evidence、StageRun、Attention、运行事件、模型调用或价格说明。值为零时仍然引用被检查的集合，而不是留下一个无法解释的裸零。

测量结果是可重建的投影，不写入十项业务对象，不改变 Delivery，也不成为另一套执行或评分权威。同一组规范化输入会产生完全相同的 JSON 结果，并且结果会深度冻结，调用方不能在内存中改写它。

## 完整度与可信度必须分开

候选失败不等于证据不可信。例如 Reviewer 和 Verifier 都引用真实测试证明候选有缺陷时：

```text
交付完整度 = failed
结果可信度 = independently-supported
```

反过来，Agent 声称成功但缺少直接证据、必需角色或当前候选发现时：

```text
交付完整度 = complete 或 incomplete
结果可信度 = insufficient
```

只有必需验收条件全部通过，并且独立角色与直接证据都完整时，才存在“完成证明”。这一判断不会把 Executor 的自报完成当成证据。

## 显示可疑的成功和失败

测量结果另外保留四项派生事实：

- 是否存在成功声明；
- 是否存在完整且独立支持的完成证明；
- 是否有“声明成功但证明不足”的风险；
- 是否有“证明已经完整但运行仍报告失败”的风险。

这两类风险不会被自动改写成成功或失败。它们只把矛盾显式显示出来，供人检查原始来源。

## 时间、用量与并行数的口径

- `runElapsedMillis` 是评估结束时间减开始时间；运行未结束时为 `null`。
- `settledStageMillis` 是所有已经结束的 `StageRun` 各自耗时之和。阶段可能重叠，因此它不是墙钟时间，也不会伪装成墙钟时间。
- token 与费用逐次从模型调用事实相加。缺少用量、价格或调用时间时，结果单独报告缺失数量；不会把未知值冒充测得的零。
- `modelElapsedMillis` 只有在所有模型调用都带完整起止时间时才给出。
- 最大并行 Agent 数按同一个 Session 内各 Agent 的首次和末次运行事件序号区间计算。它证明生命周期事件发生过重叠，不声称获得了操作系统级同时运行的精确时间。

## 确定性与真实运行分开

确定性场景只脚本化外部模型响应；真实场景连接实际 DSH 提供商路由并可能产生费用。两者使用同一个测量函数，但 `runKind` 分别固定为 `deterministic` 和 `live`。分组函数只返回两个独立列表，不提供混合平均数或合并总数。

这样可以同时验证计算逻辑和观察真实交付表现，又不会把脚本化 token、时间或成功率混入真实模型数据。

## 后果

- 完整场景的输出和每份真实 `result.json` 都带同形状的 `measures` 投影。
- 真实结果可以从原始保存事实重新计算，并与保存的投影逐字段比较。
- 失败、预算结束和中断仍然生成当时可计算的测量结果；缺失事实会明确显示为缺失或不可用。
- 产品发布门禁可以引用具体维度和来源，不需要引入一个无法解释的阈值总分。
