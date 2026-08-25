# Rust Delivery / StrongFlow 后端切换门禁

## 结论

从阶段 2.7 起，Rust Control Plane 是迁移后 Delivery 后端的唯一正式写入者。后续后端阶段只能调用 typed Control Plane 命令和查询，不能再新增 TypeScript `DeliveryStore` 写入者，也不能从 Rust differential runner 退回旧 TypeScript 业务实现。

这项结论只针对后端写入权威，不代表浏览器和 Host 已完成切换。现有 DSH Chat、StrongFlow 页面、Host CLI 和 live evaluation 仍有一组已列明的 TypeScript 过渡调用：`winwincode-9c4.16.6.3` 负责把页面接到 Control Plane，`winwincode-9c4.16.6.6` 负责删除旧 DSH/Cordis/N-API 后端、旧路由和旧 `DeliveryStore`。阶段 2.7 不把这些尚未完成的工作写成已完成。

机器可读规则位于 [`delivery-rust-cutover.rules.json`](./delivery-rust-cutover.rules.json)。本地代码索引脚本在本工作树中不存在，因此本门禁使用文件清单、`rg` 和直接读文件，只声明文件级覆盖，不声称完整调用关系覆盖。

## 唯一 Rust 后端路径

十个冻结场景由 Node 生成一份闭合的 plan v2。Rust runner 只消费这份计划，不自行读取旧 TypeScript oracle，也不自行推导 Worker terminal 状态。所有实际写操作进入真实 `ControlPlane::start_local` 实例和同一个 SQLite 存储：

- 普通 Delivery 命令进入 `commit_delivery_command`；
- Codex 阶段派发进入 `commit_delivery_execution`；
- Worker session/thread 绑定进入 `commit_delivery_session_binding`；
- Worker 终态进入 `commit_delivery_terminal_outcome`；
- 人工批准后的任务图进入 `commit_delivery_task_breakdown`；
- Reviewer/Verifier 结论进入 `commit_delivery_verdict`；
- Delivery 与 runtime 读取进入 `StrongFlowProjectionQueryPort`。

每次正式变更把当前状态、追加式 journal、同请求回执和待发布事件作为一个 SQLite 事务提交。runner 里唯一的直接 SQLite seed 只用于把旧 task-DAG 测试样本一次性转换成新身份；它不是产品写入口。重启、损坏、恢复、请求重放、修订冲突、旧候选失效、Attention、Inconclusive、InfraError 和返工都由同一真实 Control Plane 路径验证。

## 精确结果

唯一结果文件是 `tests/fixtures/oracles/delivery-strongflow-rust-expected.v1.json`，SHA-256 为 `4aaab65259218df5df814b9d9743d71e779aad81a552c0540df63ad8490f1c71`。十场景最终修订号依次为：

| 场景 | 最终修订号 | 关键结果 |
| --- | ---: | --- |
| success-closed-loop | 21 | Delivered，Pass |
| request-id-replay | 1 | 同一请求返回原结果，不重复写入 |
| revision-conflict | 2 | 旧修订号被拒绝，状态不变 |
| corruption-recovery | 1 | 损坏时拒绝读取，恢复后逐值一致 |
| task-dag | 2 | 先执行前置任务，循环任务图零写入 |
| candidate-invalidation | 31 | 旧候选被拒绝，新候选 Pass |
| attention | 8 | Attention 保留并正确结算 |
| inconclusive | 19 | Inconclusive，进入待处理状态 |
| infra-error | 19 | InfraError，进入待处理状态 |
| rework | 31 | Fail 后返工，再以新候选 Pass |

## 旧 TypeScript 边界

门禁逐文件冻结现存过渡调用。当前 `StrongFlowService` 只能在 Host CLI、StrongFlow DSH 入口和 live evaluation 创建；`DeliveryStore.create/open/append` 只能留在 StrongFlow service 与 DSH restart recovery；`DeliveryStore` 的公开导出只允许留在 StrongFlow 包入口，供这条过渡恢复路径使用。任何新增文件命中这些写入口都会让门禁失败。

这组文件不是后续后端阶段的备用路径，也不允许双写或 fallback。它们只服务尚未切换的页面/Host 和旧行为 oracle，并由阶段 6 的两个明确任务一次性迁移、删除。

## 资源清理与恢复

`scripts/verify-rust-delivery-cleanup.mjs` 默认重复四次完整 Rust differential。每次都在新进程和独立 `TMPDIR` 中运行，要求十场景逐值匹配，并在进程结束后检查该临时目录为空。每轮都包含真实 Control Plane shutdown/restart、请求回放，以及 journal 损坏/恢复场景。

`verify:fixture-cleanup` 同时运行旧行为样本清理、Rust Control Plane 清理和 native kernel 清理。可用 `WINWINCODE_CLEANUP_STRESS_ITERATIONS=32` 提高重复次数。

## 运行方式

```bash
corepack pnpm verify:delivery-rust-cutover
corepack pnpm verify:fixture-cleanup
corepack pnpm verify
```
