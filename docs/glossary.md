# WinWinCode 术语表

## 进程与端口

| 术语 | 含义 | 源码依据 |
| --- | --- | --- |
| Client | 浏览器中的 WinWinCode 前端。它读取 Server 投影并提交命令。它不承担执行授权或持久业务状态写入。不要把这里的 Client 与协议中表示执行设备的 `ClientNodeId` 混用。 | `apps/client/src/application.ts`；`apps/client/src/community-control-plane-client.ts` |
| Device Client | 运行在执行设备上的守护进程。它登记设备、维护占用镜像、同步仓库绑定、监管本地 Worker、保留 Candidate，并通过 ClientControlPort 与 Server 通信。 | `crates/winwincode-device-client/src/daemon.rs` 的 `DeviceDaemon`；`crates/winwincode-device-client/src/supervisor.rs` |
| Server | 产品控制面。它处理浏览器 API、命令、投影、调度、持久化、Worker 连接和 Device Client 连接。 | `crates/winwincode-server/src/main.rs`；`crates/winwincode-server/src/application.rs` |
| Worker | 执行端进程。它在租约和 fencing 约束下运行 WorkRun，驱动 Codex Core，执行工具，并把事件、工件和终态送回控制面。 | `crates/winwincode-worker/src/main.rs`；`crates/winwincode-worker/src/lib.rs` |
| kernel-helper | 产品内部辅助进程 `winwincode-kernel-helper`。它提供 Code Mode 主机入口和受控的本地执行能力。 | `crates/helper/src/main.rs`；`crates/winwincode-codex/src/adapter.rs` 的 `SEALED_HELPER_NAME` |
| Codex Core | 内嵌在产品中的上游 Codex 执行内核。第一方 `crates/kernel` 通过 `codex-core-api` 封装线程、回合、审批、工具和恢复能力。 | `crates/kernel/src/lib.rs`；`third_party/codex/codex-rs/core-api/src/lib.rs` |
| ExecutionPort | Server 控制面与 Worker 之间的传输中立契约。它覆盖注册、Job、租约、运行时事件、模型流、输入、审批、工件、动作和终态。 | `schema/winwincode/v1/execution-port.schema.json`；`crates/winwincode-execution-port/src/generated.rs` 的 `ExecutionPortMessage` |
| ClientControlPort | Server 与 Device Client 之间的传输中立契约。它覆盖设备登记、连接码、占用、仓库投影、Worker 监管和 Candidate 交付。 | `schema/winwincode/v1/client-control.schema.json`；`crates/winwincode-client-port/src/messages.rs` |

## 会话与执行身份

| 术语 | 含义 | 源码依据 |
| --- | --- | --- |
| ProductSession | 一次可执行对话的持久身份和状态机。它绑定 project、repository 和模型路由。Chat 与 Delivery 执行都以它为产品会话锚点。 | `crates/winwincode-session/src/product_session.rs` 的 `ProductSession`、`ProductSessionState` |
| WorkerSession | Device Client 启动并监管的一次 Worker 会话身份。它把执行任务绑定到具体 Worker 进程实例。替换 attempt 必须使用新的 WorkerSession。 | `crates/winwincode-storage/src/worker_session_slots.rs` 的 `WorkerSessionSlots`；`crates/winwincode-domain/src/generated.rs` 的 `WorkerSessionId` |
| CodexThread | Codex Core 中的线程对象。WinWinCode 用 `CodexThreadId` 把 Core 线程绑定到 WorkerSession 和 ProductSession。重新开始的 attempt 使用新的 CodexThread。 | `third_party/codex/codex-rs/core-api/src/lib.rs` 的 `CodexThread`；`crates/winwincode-codex/src/contract.rs` 的 `CodexThreadSession` |
| WorkItem | Delivery 中的一项工作安排。它引用 WorkContract 修订、目标、验收标准和依赖关系。 | `crates/winwincode-domain/src/generated.rs` 的 `WorkItem`、`WorkItemState` |
| WorkRun | WorkItem 的一次执行尝试。它记录 attempt、ExecutionJob、租约、Worker 身份、Candidate 和终态。 | `crates/winwincode-domain/src/generated.rs` 的 `WorkRun`、`WorkRunState` |
| requestId | 命令的幂等身份。持久回执实际按 actor、作用域和 `requestId` 查找。同一身份只允许重放同一命令摘要。 | `crates/winwincode-storage/src/lib.rs` 的 `ReceiptIdentity`、`replay_receipt` |
| expectedRevision | 命令声明的当前状态修订。存储只在它等于实际修订时写入，并把修订增加一。 | `crates/winwincode-storage/src/lib.rs` 的 `append_state_commit`；`crates/winwincode-api/src/generated.rs` 的 `CommandEnvelope` |
| fencing | 用单调变化的 token 和完整租约身份拒绝旧执行者。新 attempt、Worker replacement 或占用强制切换后，旧 Worker 的迟到消息不能落地。 | `crates/winwincode-storage/src/execution_registry.rs`；`crates/winwincode-device-client/src/fencing.rs` 的 `FencingGuard` |

## 交付与验证

| 术语 | 含义 | 源码依据 |
| --- | --- | --- |
| Delivery | 从规格到交付终态的聚合根。它包含 DeliverySpec、WorkItem、WorkRun、Candidate、Attention、Evidence 和 Verdict。 | `crates/winwincode-delivery/src/domain/mod.rs` 的 `Delivery`、`DeliverySnapshot` |
| Candidate | WorkRun 产出的冻结代码结果。其身份绑定 Candidate ref、commit、tree 和 diff 摘要。验证和发布必须读取同一冻结 Candidate。 | `crates/winwincode-domain/src/generated.rs` 的 `Candidate`；`crates/winwincode-delivery/src/domain/candidate.rs` 的 `FrozenDeliveryCandidate` |
| Publication | 把通过策略检查的 Delivery Candidate 发布到指定目标的状态机。Publication 是 Candidate 的读取者；它到终态前会阻止 Candidate Git ref 释放。 | `crates/winwincode-publication/src/coordinator.rs` 的 `Publication`；`crates/winwincode-control-plane/src/candidate_git_release.rs` 的 `ensure_publication_readers_closed` |
| StrongFlow | WinWinCode 的固定角色交付工作流和对应投影。角色包括需求、方案、计划、执行、独立评审、独立验证、对抗验证和修复。 | `packages/contracts/src/strongflow-role.ts` 的 `STRONGFLOW_ROLE_IDS`；`crates/winwincode-control-plane/src/strongflow_projection.rs` |
| Fusion | 对多个 Provider 候选进行盲化、claim 提取、共识和冲突分析的机制。Fusion 结果是调查和判定输入。它本身不证明 Candidate 质量通过。 | `crates/winwincode-fusion/src/analysis.rs` 的 `FusionClaim`、`FusionCandidateClaims`；`crates/winwincode-fusion/src/claims.rs` |
| JEV | 对 premise 与 hypothesis 输出 entailment、contradiction、neutral 分数的判断机制。代码使用 OpenJev 运行时。分数是分析信号，不是验收回执。 | `crates/winwincode-provider/src/jev.rs` 的 `JevHypothesis`、`JevScores`、`JevRuntime` |

## 设备与界面

| 术语 | 含义 | 源码依据 |
| --- | --- | --- |
| Readiness | Client 的开工前检查投影。它汇总作用域、模型路由、凭据、Worker、helper 和首个 Chat 交付等条件，并给出 `ready`、`attention`、`blocked` 或 `unavailable`。 | `apps/client/src/readiness-view-model.ts` 的 `ReadinessItemId`、`ReadinessReason` |
| Occupancy | 用户对执行设备的排他占用。确认后，Device Client 才能凭 `occupancyLeaseId` 和 fencing token 启动 Worker 会话。 | `crates/winwincode-control-plane/src/client_occupancy.rs`；`crates/winwincode-storage/src/client_occupancy.rs` |
| Attention | 挂在 Delivery 上的待处理事项。“我的工作”任务看板中的筛选是正式入口。 | `crates/winwincode-delivery/src/domain/attention.rs` 的 `AttentionItem`；`apps/client/src/attention-center-view-model.ts` |

## 作用域

当前实现使用 organization、workspace、project、repository 四级作用域。`OrganizationId`、`WorkspaceId`、`ProjectId`、`RepositoryId` 都是当前协议和存储中的真实身份。`RepositoryScope` 携带完整四级键。

源码依据：`crates/winwincode-domain/src/generated.rs` 的 `RepositoryScope`；`crates/winwincode-storage/src/lib.rs` 的 `receipt_scope_key`；[ADR-0004](adr/0004-scope-model.md)。
