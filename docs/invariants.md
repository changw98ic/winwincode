# 运行时与持久化不变量

本页记录执行和持久化必须保持的约束，并列出对应实现与测试。

## 命令身份与修订

### `requestId` 只标识同一 actor 和作用域内的一次命令

约束：命令回执的身份由 actor、作用域和 `requestId` 共同组成。同一身份与同一命令摘要可以重放原回执。同一身份不能换成另一份命令内容。重放不能再次修改状态或再次产生副作用。

源码依据：

- `crates/winwincode-storage/src/lib.rs`：`ReceiptIdentity`、`commit_in_transaction_with_claim_authority`、`replay_receipt`、`StorageError::request_conflict`。
- `crates/winwincode-control-plane/src/delivery_transaction.rs`：`validate_command` 要求命令 `requestId` 与待提交的 Delivery 执行完全一致。

相关测试：`tests/api-production-vertical.test.mjs`、`tests/chat-execution-recovery-production.test.mjs`、`crates/winwincode-control-plane/tests/delivery_atomic_transaction.rs`。

### `expectedRevision` 是写入前置条件

约束：新写入只接受当前修订。成功写入把修订增加一。修订不匹配必须返回冲突。状态 guard、附加状态变更和聚合日志也必须在同一事务内检查各自的预期修订。

源码依据：

- `crates/winwincode-storage/src/lib.rs`：`append_state_commit`、`check_state_revision_guards`、`check_state_mutation_revisions`。
- `crates/winwincode-control-plane/src/delivery_transaction.rs`：`validate_command` 要求待提交 Delivery 的修订等于 `expectedRevision + 1`。

相关测试：`crates/winwincode-control-plane/tests/delivery_atomic_transaction.rs`、`tests/control-plane-http-contract.test.mjs`。

## 先持久化，再传输

约束：状态、命令回执、聚合日志记录和 outbox 事件在一个数据库事务中提交。调用方只能发送已经进入持久 outbox 的消息。发送尝试不能替代持久化。只有与原消息匹配的 ACK 才能推进或删除 outbox 项。

源码依据：

- `crates/winwincode-storage/src/lib.rs`：`append_state_commit` 依次写状态、日志、回执和 outbox，并由 `SqliteStorage::commit` 提交同一事务。
- `crates/winwincode-control-plane/src/delivery_execution.rs`：`commit_and_dispatch` 先调用 `commit_delivery_and_job_intent`，再派发，最后用 `mark_job_dispatched` 标记持久 outbox 事件。
- `crates/winwincode-codex/src/adapter.rs`：`take_execution_messages` 在返回消息前调用 `ExecutionOutbox::retain_in_transaction`。
- `crates/winwincode-codex/src/outbox.rs`：`ExecutionOutbox::acknowledge_response`、`apply_runtime_ack`。
- `crates/winwincode-client-port/src/exchange/outbox.rs`：`OutboxSession::enqueue`、`acknowledge`、`compact_through`。

相关测试：`crates/winwincode-control-plane/tests/delivery_atomic_transaction.rs`、`crates/winwincode-codex/src/storage_mechanism_regression.rs`、`tests/chat-execution-ack-loss-production.test.mjs`、`tests/chat-execution-transport-production.test.mjs`。

## Device 绑定与身份恢复

约束：Device 在首次请求前持久化 Ed25519 私钥、Server URL、稳定请求 ID、配对令牌和绑定意图。Server 原子保存令牌消费、设备绑定、管理授权和回执。丢失成功响应或在保存本地回执前崩溃后，Device 使用原请求与原私钥恢复同一绑定。

约束：Device 重启使用已保存的 Server URL 和身份。一次性配对令牌只用于建立绑定。每次签名使用新的单次挑战。已吊销绑定不能通过旧请求、重启或旧令牌重新激活。

约束：Device 吊销必须在保存吊销事实的同一事务内收尾该 Device 的执行资源。系统按原 Worker 和 Worker instance 的组合关闭全部 running 或 cancelling slot，包括旧 attempt 的 slot。仍属于该实例的 Worker 标记为断开。系统同时收尾当前租约及其非终态 Job 和 admission reservation。原 Worker 不能再获得新任务。已有终态和其他 Device 的资源保持不变。无法确认的 tokens 和 cost 保持 unknown。reconciliation 保持 pending，并保留已知 tokens 下界和 Server 已观测的运行时。

源码依据：`crates/winwincode-device-client/src/remote_auth.rs::bind_device`、`crates/winwincode-storage/src/device_auth.rs::bind_device_identity`、`revoke_device_identity`、`settle_revoked_executions`、`crates/winwincode-storage/src/worker_session_slots.rs::fence_device_worker_sessions_in_transaction`、`crates/winwincode-server/src/device_auth.rs::DeviceAuthApplication`。

相关测试：Device `remote_auth::tests`；Server `device_auth::tests`；Storage `device_auth::tests::token_binding_access_and_receipt_roll_back_together`、`revoke_closes_running_and_unaccepted_leases_without_changing_success_or_other_devices`。测试支持钩子与证据边界见 [远程 Device 验证](testing.md#远程-device-验证)。

## 租约与 fencing

约束：执行权限绑定完整租约身份。至少包括 Job、attempt、lease、fencing token、Worker、Worker instance 和 WorkerSession。终态、取消 ACK、运行时事件、模型帧和工具动作必须匹配当前权限。仅凭 `jobId`、`workerId` 或较新的时间戳都不能授权写入。

Device 占用和 Worker 执行使用不同的租约域。新的 Device 占用使用更高的 token。Device 的 `FencingGuard` 只接受与本地 occupancy mirror 完全相同的 lease ID 和 token。较低 token、未见过的较高 token 或其他 lease 均拒绝。mirror revision 变化会使原本地 ticket 失效。

Device 转发子 Worker 消息时绑定 Client Node、Worker、Worker instance 和启动授权。Server 核对当前 placement。任务结果与用量结算再次核对当前执行身份和会话状态。

远程交换按本次响应的 `serverTime` 计算剩余租约。Worker 在 relay 调用前记录 `request_started`，使用 `request_started + max(expiresAt - serverTime, 0)` 作为本地单调时钟截止时间。relay 排队和网络耗时消耗同一预算。截止已过的派发或续租不能进入控制队列；取出控制时再次核对。派发重放不延长截止时间。续租只有被当前执行接受后才更新权限。

Server 在同一当前时间核对帧自身期限、当前 Job 租约、完整 lease 身份和已接受的历史派发或续租 period。旧 fencing、未知 period、已过期或终态租约的派发与续租从出站队列移除。保留帧沿用原始字节；本次响应时间不能改写持久化帧的发送时间。

已经在原租约期合法进入 durable outbox 的 Artifact 原帧，在续租后仍按该已接受租约期校验。Server 必须把原 period 与当前续租链关联，并继续核对 Job、payload、Worker、Worker instance、attempt、fencing token 和 SessionBinding。未接受的 period 或外来身份必须失败关闭。续租不能改写原帧或其中的 lease。Server 不能仅因续租拒绝原 final Artifact 帧。ACK 回显原 lease；精确重放按重复写处理。

源码依据：

- `crates/winwincode-storage/src/execution_registry.rs`：`ExecutionDispatchAuthority`、`validate_fencing_token`、终态与派发校验。
- `crates/winwincode-control-plane/src/delivery_execution.rs`：`acknowledge_job_cancel` 比较请求、完整租约和 WorkerSession。
- `crates/winwincode-control-plane/src/runtime_event_transaction.rs`：`validate_authority` 及 `Rejection::StaleFencingToken`、`Rejection::WorkerInstance`。
- `crates/winwincode-device-client/src/fencing.rs`：`FencingGuard::verify_ticket` 在占用镜像缺失或票据过期时失败关闭。
- `crates/winwincode-codex/src/model_bridge.rs`：`SharedAuthoritySource::install_run_binding`、`validate_invocation`。
- `crates/winwincode-server/src/remote_worker_transport.rs`：远程出站租约筛选与响应时钟。
- `crates/winwincode-worker/src/remote_transport.rs`：`SharedRemoteState::receive_response` 的本地截止时间与重放校验。
- `crates/winwincode-control-plane/src/execution_lease_period.rs`：`accepted_lease_window_matches` 核对已接受的续租链。
- `crates/winwincode-control-plane/src/artifact_transaction.rs`：`ArtifactMessageContext::from_authority`、`validate_durable` 和 `validate_authority`。

相关测试：`tests/execution-port-contract.test.mjs`、`tests/chat-execution-obsolete-owner-production.test.mjs`、`crates/winwincode-device-client/src/fencing.rs` 的单元测试、`crates/winwincode-control-plane/tests/session_binding_transaction.rs`、`crates/winwincode-control-plane/tests/durable_execution_port_ingress.rs`。

## 崩溃恢复与新 attempt

每次模型调用和工具调用都保留请求正文与结果正文。执行前允许先保存请求及明确的 pending 状态。成功、失败和取消的终态必须把请求与结果成对原子持久化。

约束：可信的 Worker 崩溃或退出后，系统保存真实退出事实，并将该 owner 的 pending 模型调用和工具调用闭合为失败。原请求和失败结果必须保留。已发操作的用量或副作用无法确定时，应记录 unknown 或 uncertain，不能伪造结果或用量。

约束：不可转移的 Codex 线程恢复为新 attempt。新 attempt 使用新的 Worker instance、WorkerSession、CodexThread、租约和更大的 fencing token。它保留原始产品请求身份，但不能把前一 attempt 的活权限带入新进程。

源码依据：

- `crates/winwincode-control-plane/src/recovery_router.rs`：`SessionRecoveryPlan::StartFreshAttempt`、`WorkerReplacementPlan`、`RecoveryRouter::recover_session`。
- `crates/winwincode-control-plane/src/product_session_service.rs`：执行替换要求 `previous_attempt + 1`。
- `crates/kernel/src/lib.rs`：`recover_interrupted_tool_calls`。
- `crates/winwincode-codex/src/store.rs`：`close_unmeasured_primary`、`close_pending_jev_parent`。
- `crates/winwincode-codex/src/adapter.rs`：`interrupt`、`poll` 的中断取消恢复分支。

相关测试：`tests/chat-call-pairs-production.test.mjs`、`tests/chat-execution-recovery-production.test.mjs`、`crates/winwincode-delivery/tests/workrun_replacement.rs`。

## 旧 Worker 的迟到消息

约束：替换成功后，拒绝旧 Worker 的全部迟到消息，包括原消息重发，且不写入业务状态、前一 attempt、后继 Codex 状态或用量事实。已有持久事实的终态 ACK 和清理按原身份处理。这类清理不能重新接受旧 Worker 消息、恢复旧权限或启动新的 Provider 调用。

源码依据：

- `crates/winwincode-codex/src/model_bridge.rs`：`SharedAuthoritySource::detach_run`、`renew_retained_binding`、`acknowledge_consumed_terminal_without_owner`。
- `crates/winwincode-storage/src/lib.rs`：`commit_with_execution_authority_guard_in_transaction` 要求 Job、派发权限和 Worker instance 仍是当前值。
- `crates/winwincode-control-plane/src/runtime_event_transaction.rs`：后继 attempt 建立后拒绝前驱权限。

相关测试：`tests/chat-execution-obsolete-owner-production.test.mjs`、`tests/chat-call-pairs-production.test.mjs`、`crates/winwincode-codex/src/model_ack_transport_identity_tests.rs`。

## Candidate 的读取者与释放

约束：冻结 Candidate 在 Delivery、验证 WorkRun 和 Publication 仍可能读取时必须保留。只有 Delivery 已到终态、所有 WorkRun 已到终态、所有关联 Publication 已到终态，并且读关闭回执已经持久化后，系统才可删除稳定 Candidate Git ref。

约束：释放权限绑定 Delivery、终态回执摘要和读关闭回执摘要。释放前必须再次核对 ref 仍指向已固定的 Candidate commit。释放意图和释放结果可在重启后幂等恢复。

源码依据：

- `crates/winwincode-control-plane/src/candidate_git_release.rs`：`commit`、`ensure_publication_readers_closed`、`validate_release_authority`。
- `crates/winwincode-storage/src/git_candidate_retention.rs`：`CandidateGitReleaseAuthority`、`CandidateGitRetention::release_after_delivery_final`、`retain_release_intent`。

相关测试：`crates/winwincode-control-plane/tests/candidate_git_release_vertical.rs`、`crates/winwincode-storage/tests/git_candidate_retention.rs`、`crates/winwincode-control-plane/tests/session_binding_transaction.rs`。

## 候选应用与恢复

约束：候选的 `baseCommit` 来自生成时冻结的源码链。多轮修改沿该链追溯原始基线。Device 拒绝缺失或成环的来源。应用时，目标分支 HEAD 必须仍等于该基线，工作树必须干净，候选摘要必须匹配授权。基线变化要求重新准备候选与审批。

Server 在同一事务中持久化应用授权、审批事件和下行命令。重试授权也与对应下行命令一并提交。入队失败时，授权和事件一并回滚。`applicationId` 和 `approvalId` 绑定设备、仓库、候选、基线、目标分支、策略与候选摘要。摘要使用 `sha256:` 加规范 JSON `[repository_binding_id, base_commit, candidate_commit]` 的摘要。摘要用于关联候选，验证结论仍须来自独立执行证据。

Device 必须先把带 `started_at` 的应用记录写入 `candidate_applications` 并持久化，再写 Git。应用提交携带 `WinWinCode-Application: <application_id>` 尾注。完成回执先进入本地 journal，再进入持久 outbox。

Device 重启后核对原基线到目标分支 HEAD 之间的应用尾注。找到原应用提交时补记结果。目标分支仍在原基线、工作树干净且没有残留应用工作区时，记录 `not_started`。其他无法确定的结果记录为 `outcome_unknown`。

Device 启动完成日志核对后，在有效的当前占用授权下主动补报。每次新的占用 offer 安装本地 mirror 后，也执行相同补报。journal 保存已进入持久 outbox 的 `reported_epoch`，避免同一 epoch 重复补报。旧 instance 或旧占用下的应用命令返回拒绝 ACK 并消费游标，使后续恢复授权能够到达。

恢复事实通过当前占用授权下的 `client.application.reconciled` 上报。Server 同时核对原审批与候选绑定。旧 epoch 的普通 `apply_result` 只写事实。候选已到终态时，补报保留真实应用事实。`not_started` 进入 `awaiting_retry`，用户再次申请后，Server 才能在当前租约下授予同一应用的 `reconciledRetry`。`outcome_unknown` 进入 blocked，后续应用请求返回 `409 WRONG_STATE`。

应用请求超时或仍显示 `applying` 时，用户可以选择“恢复原应用”。请求保留原候选、目标分支和基线，Server 复用原 `applicationId` 与 `approvalId`，向当前 Device 实例重发。Device 已有本地应用记录时，根据 journal 和 Git 事实裁决：已完成则返回原结果，确认未开始则补报 `not_started`，结果不明则补报 `outcome_unknown`。本地尚无该应用记录时，Device 按当前授权执行原请求。单条命令的拒绝 ACK 不能证明整个应用尚未开始。

源码依据：`crates/winwincode-device-client/src/candidate_registry.rs`、`application_journal.rs`、`apply_engine.rs`；`crates/winwincode-storage/src/candidate_applications.rs`；`crates/winwincode-server/src/client_candidates.rs`。相关测试位于 Device 的 `tests/apply_engine.rs` 和 Server 的 `tests/client_candidates.rs`。

## 验证必须来自真实观察

约束：executor 和 remediator 不能验证自己的 Candidate。reviewer、verifier 和 adversarial-verifier 使用只读 Candidate 工作区。每个结论必须引用实际保存的 Command 或 Test 事件。验证命令必须按验收标准指定的方法独立运行。模型生成的文字、未执行的检查、检查命令的局部输出或旧回执不能单独证明通过。

约束：Fusion 和 JEV 只提供分析信号。Fusion 的共识或 JEV 分数不等于验收通过。Delivery 的判定仍须绑定冻结 Candidate、验收标准和可验证证据。

源码依据：

- `crates/winwincode-codex/src/stage_product.rs`：`canonical_role`、`PreparedStageProduct::from_job` 的 verifier 规则。
- `crates/winwincode-codex/src/workrun_runtime_projection.rs`：`WorkRunRuntimeProjector::verification_repair_prompt`。
- `crates/winwincode-delivery/src/domain/verification.rs`：`validate_independent_verification`。
- `crates/winwincode-delivery/src/domain/verdict.rs`：Candidate 与 Evidence 的判定校验。

相关测试：`crates/winwincode-codex/src/tool_fact_projection_tests.rs`、`crates/winwincode-delivery/src/domain/verification.rs` 的单元测试、`tests/strongflow-fusion-production.test.mjs`。

## 取消与恢复

约束：取消先关闭当前 root 和子线程的活权限，再执行较慢的 Provider、协议和 Core 清理。取消意图必须持久化。重启后只能继续回执、工件和用量清理，不能重新启动已取消的 Core 回合。

约束：已保留的 Candidate 工件在取消意图提交后不能恢复上传。已经消费的原始终态仍可按原身份重放 ACK。

Worker 的独立终止监测每 25 毫秒检查一次终止错误与本地租约截止时间。当前 managed Worker 为完整 `shutdown` 等待最多 10 秒；未确认执行清理完成时，再为 Core `stop_execution` 等待最多 5 秒。租约到期且执行已停止后，Worker 再用最多 5 秒重放原持久化终态并处理对应业务 ACK。此时执行权限保持到期；未收到 ACK 的终态保留在 outbox。Device supervisor 的生产停止宽限为 20 秒。Device 服务控制轮询为 200 毫秒，单次控制 HTTP 仍可能占用 10 秒。

这些数值分别表示检测周期和等待预算。完整进程退出以 Worker、已登记后代和工具独立进程组的实际退出证据为准。常规关闭、租约到期和突发 `SIGKILL` 分别验证。进程归属核对同时使用 PID 和启动身份；无法确认身份时报告未确认状态。

Unix 受管 pipe、PTY、legacy shell、原生验证、MCP 子进程、hook、Shell 快照和 Git 子进程通过私有 lifeline 管道维持 owner 归属。owner 退出使管道写端关闭，guardian 随后回收同一受管工具进程组。Linux sandbox、工具主动创建新会话或 PTY job control 创建新进程组的情形需要独立验收。

源码依据：

- `crates/winwincode-codex/src/model_bridge.rs`：`ExecutionPortModelBridge::fence_binding`、`cancel_thread`。
- `crates/winwincode-codex/src/adapter.rs`：`fence_thread`、`interrupt`、`poll`。
- `crates/winwincode-codex/src/candidate_artifact_outbox.rs`：`request_cancel`、`cancel`、`delivery_allowed`。
- `crates/winwincode-worker/src/main.rs`：`shutdown_worker`；`remote_transport.rs`：`wait_for_terminal_error`。
- `crates/winwincode-device-client/src/service.rs` 和 `supervisor.rs`：控制轮询、停止宽限和进程身份核对。
- `third_party/codex/codex-rs/utils/pty/src/process_group.rs`：受管工具进程组与 lifeline guardian。

相关测试：`crates/winwincode-codex/src/model_local_cancellation_tests.rs`、`crates/winwincode-codex/src/candidate_artifact_outbox.rs` 的 `cancellation_intent_survives_restart_and_blocks_candidate_replay`、`tests/code-mode-cancellation-evidence.test.mjs`。

## 作用域

约束：当前协议与存储必须完整校验 organization、workspace、project、repository 四级作用域。命令回执、Worker 管理、仓库绑定、执行权限和审计事件不能跨作用域复用。

源码依据：

- `crates/winwincode-storage/src/lib.rs`：`receipt_scope_key`。
- `crates/winwincode-storage/src/execution_registry.rs`：`validate_worker_scope`、`worker_scope_key`。
- `schema/winwincode/v1/control-plane-http.schema.json`、`schema/winwincode/v1/domain.schema.json` 与 `schema/winwincode/v1/execution-port.schema.json`。

作用域简化需要同时更新 schema、生成代码、Server、Worker、Device Client、持久化和测试。不能仅改名称或在单一层丢弃归属字段。多个项目的归属、执行授权和持久化隔离必须一起验证。产品目标见 [产品范围](architecture/overview.md#产品范围)；协议变更见 [生成文件规则](contracts.md#生成文件规则)。

相关测试：`tests/control-plane-http-contract.test.mjs`、`tests/execution-port-contract.test.mjs`、`crates/winwincode-server/tests/application_composition.rs`。

## 当前数据库 schema

已批准的开发目标适用于未发布结构。新改动不再为旧的未发布 schema 增加自动升级、回填或兼容路径。不兼容结构应拒绝打开。实现这个目标时必须保留已有数据和失败现场。不得为了启动而删除表、清空库或覆盖用户数据。处理现有迁移需要独立实施任务。

已发布用户数据的升级政策尚未定义。这个开发目标不能代替后续发布政策。备份只提供恢复点，不提供 schema 升级。操作见 [部署指南](operations/deploy.md#备份校验和恢复)。

本节核对的严格初始化和结构校验入口有两个：

- `crates/winwincode-storage/src/lib.rs` 的 `SqliteStorage::open` 及其调用的 `initialize_schema`。
- `crates/winwincode-device-client/src/store.rs` 的 `DeviceStore::open` 及其调用的 `initialize_schema`。

当前 Server 存储版本为 `8`，Device 存储版本为 `11`。进程归属、执行租约和回执消费记录分别由对应存储保存。

这两个函数先取得 `TransactionBehavior::Immediate` 事务，再读取 SQLite `user_version`。值为 `0` 时，函数创建当前结构并写入当前版本。值为当前常量时，函数校验既有结构。其他值返回 `unsupported schema version <version>`。初始化、版本写入、结构校验和提交位于同一事务。`SqliteStorage` 调用 `validate_journal_schema` 等校验函数。`DeviceStore` 调用 `validate_store_schema` 和 `validate_worker_report_schema`。这些实现依据不能推广为所有产品数据库的保证。

`crates/winwincode-codex/src/adapter.rs` 的 `ProductionCodexAdapter::open` 调用 `AdapterStore::open`。`crates/winwincode-codex/src/store.rs` 的 `AdapterStore::open` 仍会初始化、迁移和回填旧结构。

这里的 `initialize_schema` 不检查 `user_version`。例如，`migrate_mcp_approval_schema` 在旧约束缺少 `mcp` 时新建表，复制 `approval_operation` 记录，删除旧表，再重命名新表。打开流程还包含模型游标、outbox 索引、调用阶段和输入身份的迁移或补列，以及 `migrate_stored_run_role_policies_v1_to_v2`。

工件存储也有迁移。`CandidateArtifactOutbox::open` 调用 `migrate_candidate_content`。`DiagnosticArtifactOutbox::open` 通过 `prepare_diagnostic_job_route` 迁移诊断路由。

内嵌 Codex 也保留迁移路径。`crates/kernel/src/lib.rs` 的 `Kernel::initialize_runtime` 调用 `init_state_db`。`third_party/codex/codex-rs/state/src/runtime.rs` 的 `StateRuntime::init` 打开并迁移多个 SQLite 库。迁移集合包含子代理线程关系使用的 `0021_thread_spawn_edges.sql`，并创建回填状态记录。

这些路径是已批准开发目标的现有例外。重新打开旧 Codex 数据目录可能在处理新业务命令前写入 schema、迁移标记、索引、回填字段或持久记录。部分迁移各自在事务中提交。后续打开步骤失败时，先前已经提交的迁移可能保留。因此，当前实现不提供一次打开覆盖全部 Codex 数据库的原子拒绝保证。迁移清理由对应实施任务处理。

测试入口：`crates/winwincode-storage/tests/sqlite.rs` 的 `startup_rejects_a_database_from_a_newer_schema_version`、`unpublished_schema_versions_are_refused_without_rewriting_state_or_receipts` 和 `initial_schema_failure_rolls_back_before_serving`；`crates/winwincode-device-client/src/store.rs` 的 `initialization_reads_version_after_acquiring_the_database_lock`；`crates/winwincode-codex/src/store.rs` 的 `mcp_approval_migration_preserves_existing_pending_and_resolved_records`；`crates/winwincode-codex/src/storage_diagnostic_migration_tests.rs` 的 `diagnostic_job_route_migration_preserves_bytes_and_reopen_skips_history`、`diagnostic_job_route_legacy_fault_rolls_back_all_schema_and_bytes` 和 `diagnostic_job_route_marker_refuses_weakened_layout`。

运行时的 pending 调用闭合和新 attempt 遵循 [崩溃恢复](#崩溃恢复与新-attempt)。它们继续保留真实调用事实。
