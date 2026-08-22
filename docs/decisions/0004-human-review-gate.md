# ADR-0004：人工批准必须经过认证边界并先落盘

- 状态：已接受
- 日期：2026-08-21
- 对应任务：`winwincode-9c4.2.5`
- 实现：[`packages/strongflow/src/human-review-gate.ts`](../../packages/strongflow/src/human-review-gate.ts)

## 结论

需求、方案、系统架构图和流程流转图全部完成后，StrongFlow 停在 `AWAITING_HUMAN_REVIEW`。此时不启动 Codex 会话、不提交模型请求、不运行角色，也不使用轮询消耗模型额度。

只有 `StrongFlowHumanReviewGate` 可以把面向人的操作转换成作业事件。它接受两种入口：

- 已认证的本地 DSH 界面会话，事件通道记录为 `local-ui`；
- 用户显式执行的本地 CLI 操作，事件通道记录为 `cli`。

后续界面和 CLI 都必须通过同一个本地作业服务调用该门禁。CLI 不直接改事件文件，模型角色和工具也不获得门禁认证能力。

## 认证边界

门禁依赖一个窄的 `HumanReviewAuthenticator` 接口。界面或 CLI 适配器把自己的不透明认证材料交给认证器，认证器只返回已经确认的 `reviewerId`。认证材料不会进入事件、快照、日志或错误信息。

```text
本地 UI 会话 ─┐
               ├─ authenticator ─ reviewerId ─┐
显式 CLI 操作 ─┘                              │
                                              ├─ HumanReviewGate ─ JobStore
模型角色 / 工具 ───── 没有认证能力 ────────────┘
```

请求必须携带完整的四个定义标识。认证成功并不代表请求一定有效；门禁随后还会核对作业确实在等待审核，而且屏幕或 CLI 读取到的四个标识仍是当前版本。

## 决定合同

持久化后的 `HumanReviewRecord` 使用
[`ADR-0013`](0013-canonical-strongflow-artifacts.md) 规定的正式制品格式，包含：

- 作为 `artifactId` 的 `HumanReviewId` 和所属 `jobId`；
- `producer` 中已认证的人工身份，以及 `local-ui` 或 `cli` 通道；
- 按固定顺序列出的 `RequirementId`、`SolutionId` 和两项 `DiagramId` 来源；
- `payload` 中的 `approved`、`changes-requested` 或 `rejected`；
- 决定时间、意见，以及退回决定所需的 `requirements`、`solution` 或 `diagrams` 范围；
- 明确为 `null` 的内核事件范围，因为人工决定不是模型回合产物。

请求不能自报审核人。审核人只取自认证器结果，事件来源身份必须与该审核人相同。角色来源即使能构造相同数据，也会在状态转换层被拒绝。

## 先持久化，再恢复

提交顺序固定为：

1. 检查请求结构和入口通道；
2. 认证真人并取得审核人身份；
3. 从作业存储重新读取当前快照；
4. 核对 `AWAITING_HUMAN_REVIEW` 和四个定义标识；
5. 创建下一序号的人工审核事件；
6. 通过原子追加存储写入并同步事件；
7. 取得重放后的新快照；
8. 最后才唤醒等待者并返回回执。

因此，调用者看到“批准成功”时，批准已经能在进程重启后重放。内存中的等待通知不是事实来源，事件存储才是。

`waitForDecision` 不创建计时器或模型调用。等待可以持续到人工操作或显式取消信号到来；取消等待只释放内存监听，不改变作业状态。主机重启时，控制器直接读取持久化快照：仍是 `AWAITING_HUMAN_REVIEW` 就继续等待，已经有决定就按状态继续。

## 三种结果

| 决定 | 持久化事件 | 结果 |
| --- | --- | --- |
| 批准 | `human-review.approved` | 进入 `PLANNING`，且批准只匹配该四元组 |
| 请求修改需求 | `human-review.changes-requested` / `requirements` | 清除整个定义并回到需求阶段 |
| 请求修改方案 | `human-review.changes-requested` / `solution` | 保留需求，清除方案和两张图 |
| 请求修改图 | `human-review.changes-requested` / `diagrams` | 保留需求和方案，清除两张图 |
| 拒绝 | `human-review.rejected` | 进入终态 `REJECTED` |

请求修改后，旧批准、候选结果和完成门禁都被清除。定义角色产生新的不可变标识并再次到达审核状态后，必须重新认证、重新核对四元组并产生新的人工决定。

## 并发和陈旧界面

同一待审核定义只接受一个决定。两个界面或 CLI 同时提交时，作业存储的不可覆盖事件发布保证只有一个下一序号成功；另一个返回 `REVIEW_ALREADY_DECIDED`。如果定义已经变化则返回 `STALE_DEFINITION`，不会把旧屏幕上的批准应用到新内容。

这套门禁不代替后续的本地 HTTP/CLI 身份实现。它固定的是不可绕过的领域边界：适配器必须先认证，审核必须引用当前定义，决定必须先持久化，角色和工具不能成为人工来源。
