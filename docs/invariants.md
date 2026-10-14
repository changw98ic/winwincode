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

## 租约与 fencing

约束：执行权限绑定完整租约身份。至少包括 Job、attempt、lease、fencing token、Worker、Worker instance 和 WorkerSession。终态、取消 ACK、运行时事件、模型帧和工具动作必须匹配当前权限。仅凭 `jobId`、`workerId` 或较新的时间戳都不能授权写入。

Device 占用和 Worker 执行使用不同的租约域。新的 Device 占用使用更高的 token。Device 的 `FencingGuard` 只接受与本地 occupancy mirror 完全相同的 lease ID 和 token。较低 token、未见过的较高 token 或其他 lease 均拒绝。mirror revision 变化会使原本地 ticket 失效。

Worker 凭据绑定 WorkerSession、Worker、Worker instance 和启动授权。远程连接还绑定当前 placement。任务结果与用量结算再次核对当前执行身份和会话状态。

源码依据：

- `crates/winwincode-storage/src/execution_registry.rs`：`ExecutionDispatchAuthority`、`validate_fencing_token`、终态与派发校验。
- `crates/winwincode-control-plane/src/delivery_execution.rs`：`acknowledge_job_cancel` 比较请求、完整租约和 WorkerSession。
- `crates/winwincode-control-plane/src/runtime_event_transaction.rs`：`validate_authority` 及 `Rejection::StaleFencingToken`、`Rejection::WorkerInstance`。
- `crates/winwincode-device-client/src/fencing.rs`：`FencingGuard::verify_ticket` 在占用镜像缺失或票据过期时失败关闭。
- `crates/winwincode-codex/src/model_bridge.rs`：`SharedAuthoritySource::install_run_binding`、`validate_invocation`。

相关测试：`tests/execution-port-contract.test.mjs`、`tests/chat-execution-obsolete-owner-production.test.mjs`、`crates/winwincode-device-client/src/fencing.rs` 的单元测试。

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

源码依据：

- `crates/winwincode-codex/src/model_bridge.rs`：`ExecutionPortModelBridge::fence_binding`、`cancel_thread`。
- `crates/winwincode-codex/src/adapter.rs`：`fence_thread`、`interrupt`、`poll`。
- `crates/winwincode-codex/src/candidate_artifact_outbox.rs`：`request_cancel`、`cancel`、`delivery_allowed`。

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

运行时只接受唯一当前 schema。开发策略适用于未发布结构：不为这些旧结构添加自动升级、回填或兼容路径。已发布用户数据的升级政策尚未定义；此开发约束不能代替后续发布政策。

新库原子初始化。当前存储入口在 `TransactionBehavior::Immediate` 事务中读取 SQLite `user_version`。值为 `0` 时创建当前结构并写入当前版本；值为当前常量时检查既有结构；其他值返回 `unsupported schema version <version>`。关键表、列、索引和触发器不符合规范时返回对应的 `... schema is not canonical` 错误。初始化、结构检查和版本写入在同一事务内完成。

不得为了启动而删除表、清空库或覆盖用户数据。不兼容结构需要另用新的数据目录，并保留原数据与失败现场。运行时的 pending 调用闭合和新 attempt 遵循 [崩溃恢复](#崩溃恢复与新-attempt)，继续保留真实调用事实。

备份恢复要求数据与程序的当前结构相匹配。备份本身不提供 schema 升级，操作见 [部署指南](operations/deploy.md#备份校验和恢复)。

源码依据：`crates/winwincode-storage/src/lib.rs` 的 `initialize_schema`、`validate_journal_schema`；`crates/winwincode-device-client/src/store.rs` 的 `initialize_schema`、`validate_store_schema`、`validate_worker_report_schema`；各独立存储模块的 `create_schema` 与 `validate_schema`。

相关测试：`crates/winwincode-codex/src/storage_diagnostic_migration_tests.rs`、`crates/winwincode-storage` 的 schema 测试。
