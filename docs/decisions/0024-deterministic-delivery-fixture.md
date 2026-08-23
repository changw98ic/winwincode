# ADR-0024：Delivery 测试基座只替换外部模型，不复制执行内核

- 状态：已接受
- 日期：2026-08-23
- 对应任务：`winwincode-9c4.11.1`、`winwincode-9c4.11.2`
- 测试基座：[`tests/fixtures/delivery-service-testkit.mjs`](../../tests/fixtures/delivery-service-testkit.mjs)
- 进程中断入口：[`tests/fixtures/delivery-service-checkpoint.mjs`](../../tests/fixtures/delivery-service-checkpoint.mjs)
- 验收测试：[`tests/delivery-fixture-testkit.test.mjs`](../../tests/delivery-fixture-testkit.test.mjs)
- 完整交付场景：[`tests/fixtures/full-delivery-scenario.mjs`](../../tests/fixtures/full-delivery-scenario.mjs)
- 完整场景门禁：[`tests/delivery-full-keyless.test.mjs`](../../tests/delivery-full-keyless.test.mjs)
- 产品边界：[ADR-0023](0023-canonical-delivery-ownership.md)

## 结论

端到端 Delivery 测试使用真实发布包、真实 DSH Agent 组合、内嵌 Codex Core、真实本地 Git 仓库、追加式 Delivery 存储和生产 StrongFlow 投影。测试只用脚本响应替换外部模型提供商，从而不需要密钥或网络，也不实现测试专用 Agent 循环、任务调度器或第二份执行状态。

测试基座是可重复使用的测试设施，不是 Delivery 数据模型中的第十一个业务对象。它不进入产品存储，也不改变 [ADR-0023](0023-canonical-delivery-ownership.md) 规定的所有权。

## 固定边界

测试直接导入各包构建后的 `dist` 入口，不从包内 `src` 文件绕过公开边界：

```text
脚本模型响应
      ↓
真实 DSH Agent 组合
      ↓
真实内嵌 Codex Core
      ↓
RuntimeSessionLedger 与生产语义投影
      ↓
StrongFlowService 与工作台浏览器包
```

DSH 与 Codex 的真实循环用于证明角色 Session、模型路由和内核事件能连通。需要精确制造失败、乱序或角色结论的场景时，测试把受控的 Codex 事件形状送入生产 `CodexRuntimeProjector`、`DeliveryRuntimeProjection` 和验证逻辑；测试本身不解释这些事件，也不决定阶段流转。

## 确定性输入

每个测试创建独立临时目录，并固定以下输入：

- 单调时钟和请求 ID；
- DSH 模型响应、用量和调用预算；
- 本地 Git 基线提交、候选提交、tree、Diff 和变化路径；
- Reviewer 与 Verifier 的结构化结果；
- 命令成功、任务失败、超时、策略拒绝和基础设施错误；
- 本地 Session 与 CLI 的人工身份凭据。

子进程环境会移除名称中含有 `API_KEY`、`CREDENTIAL`、`SECRET` 或 `TOKEN` 的变量。测试不访问远端服务，所有 Git 操作都在临时本地仓库中完成。

## 已证明的行为

验收测试覆盖七项公开 StrongFlow 操作中的全部六项变更操作：

```text
createDelivery
updateDeliverySpec
startStage
bindSession
resolveAttention
submitVerdict
```

每项变更都同时覆盖成功和明确失败，并额外证明：

- 需求与方案分开呈现，人工批准方案前不会进入执行；
- 旧 Spec revision 的批准被拒绝；
- 执行中图只显示浅蓝色受影响节点，不返回具体 Diff；
- 执行结束图使用黄色节点，并绑定冻结候选和精确 Diff；
- 缺失或伪造证据不能形成 Verdict；
- Reviewer 超时、Verifier 策略拒绝会得到 `infra_error` 和业务 Attention，不会误报通过；
- 事件缺少 sequence 或来自未绑定 Session 时会以稳定错误停止投影；
- Host 在人工审核点被 `SIGTERM` 结束后，可以从追加记录恢复并继续同一决定；
- 同一个恢复请求再次提交不会产生第二条变更记录；
- 重启前后从 Delivery 和运行记录生成的投影完全相同。

浏览器验证加载生产 StrongFlow bundle，并通过 DSH 的 `conversation.view` 注册结果渲染工作台。它不复制一份测试专用页面。

## 完整的无密钥交付场景

测试基座还运行一个独立子进程中的完整 Delivery。父进程移除凭据环境变量，子进程只使用脚本模型和临时本地 Git 仓库，并按真实服务操作依次完成：

```text
DeliverySpec
  ↓
第一次方案审核要求修改
  ↓
重新规划并拒绝旧审核集合
  ↓
批准新方案
  ↓
生成会使本地测试失败的候选
  ↓
Reviewer + Verifier 给出 fail
  ↓
人工确认绑定当前 Criterion、Verdict 和 Candidate 的返工说明
  ↓
remediator 生成新候选
  ↓
拒绝旧 Candidate，使用新 Evidence 重新验证
  ↓
pass Verdict
  ↓
人工批准最终交付
```

这个场景只有一个交付单元，所以执行、验证和返工使用 Delivery 级 `StageRun`，不为了满足内部实现而虚构 `DeliveryTask`。每个 Codex 或人工阶段都有真实 `SessionBinding`。测试同时检查 Codex 子 Agent 只从运行事件投影出来，没有写入 Delivery，也没有形成第二套 Agent 图。

第一次候选确实运行本地 Node 测试并失败；修正后的候选再次运行同一测试并通过。最终 Verdict 只引用当前 Spec、修正后的 Candidate 和新 Evidence。旧失败证据保留为历史，但不进入最终 CriterionResult 的证据集合。

## 运行方式

聚焦验证：

```bash
corepack pnpm build:ts
node --test tests/delivery-fixture-testkit.test.mjs
node --test tests/delivery-full-keyless.test.mjs
```

完整仓库门禁继续使用：

```bash
corepack pnpm verify
```
