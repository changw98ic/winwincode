# Control Plane 存储与生命周期门禁

- 正式决定：[ADR-0028](../decisions/0028-control-plane-worker-migration.md)
- 目标模块图：[0028-control-plane-worker-target-graph.json](../decisions/0028-control-plane-worker-target-graph.json)
- 机器规则：[control-plane-storage-lifecycle.rules.json](./control-plane-storage-lifecycle.rules.json)
- 基础对应任务：`winwincode-9c4.16.2.1`
- Delivery 原子扩展任务：`winwincode-9c4.16.2.3.1`

## 这份门禁说明什么

这是阶段 2.1 的目标门禁，不是实现完成声明。当前目标 crate 还没有出现在这个分支时，
Node 测试只检查规则、ADR-0028 和目标模块图是否一致。只要
`crates/winwincode-storage/Cargo.toml` 或
`crates/winwincode-control-plane/Cargo.toml` 出现，门禁就要求两个 crate 同时进入 Rust
workspace，检查依赖，并编译和执行约定的 Rust 集成测试。任务是否完成仍由实际 Rust
测试和 Beads 状态决定。

## 唯一写入方

Control Plane 是产品状态的唯一写入方。Worker、Web、事件发送器和存储 adapter 都不能
各自形成第二条产品状态写入路径：

```text
HTTP Command
  → Control Plane application command
  → ProductStateStorage transaction
      ├─ canonical state append
      ├─ aggregate journal append
      ├─ scoped command receipt append
      └─ outbox event append
  → commit
  → EventPublisher reads committed outbox
```

`ProductStateStorage` 是 Control Plane 内部的存储端口，不是 Web 或 Worker 协议。
业务模块可以准备状态变更和事件，但必须由 Control Plane 的 application command 在一个
事务里提交。Execution Worker 只能通过 ExecutionPort 上报运行事实，不能拿到这个存储
端口。

## 一次命令的原子提交顺序

状态、领域 journal、命令回执和对外事件必须进入同一个数据库事务，固定顺序如下：

1. `begin-transaction`
2. `validate-command-and-revision`
3. `append-canonical-state`
4. `append-aggregate-journal`
5. `append-command-receipt`
6. `append-outbox-event`
7. `commit-transaction`
8. `publish-committed-outbox-event`

提交前不得发布事件。canonical state、aggregate journal、带 actor 和完整 scope 的命令回执、
outbox 是同一原子单元；其中任何一步失败，都回滚整个事务，不能留下新状态、单独出现的
journal record、缺少事件的回执、孤立 outbox 行或已经发出的事件。
发送器只读取已经提交的 outbox，不能从内存中的待提交对象直接广播。

数据库提交成功后，即时发布仍可能失败。这时 canonical state、aggregate journal 和命令
回执保持已提交，outbox 事件保持 pending，命令结果必须明确表示“状态已提交、发布待重试”。
恢复只重试发送，不得重新执行原业务命令，也不得声称数据库写入已经回滚。

这个顺序解决两种半写状态：

- 已改状态却没有可恢复事件；
- 已通知客户端成功，数据库随后回滚。

`requestId`、`expectedRevision` 和领域校验在写状态前完成。命令回执的完整身份是
`actor + scope + requestId`，其中 scope 包含当前层级实际存在的全部组织、工作区、项目和
仓库 ID。相同身份和相同命令摘要返回已经持久化的回执；相同身份却带不同摘要时返回
幂等冲突；不同 actor 或不同完整 scope 可以各自使用同一个 `requestId`。

Control Plane 通过 canonical `CommandEnvelope` 生成这组身份和命令摘要。JSON 对象字段的
书写顺序不改变摘要。存储只收到带类型的身份键和 SHA-256 摘要，不保存命令 payload、
原始凭据或认证证明。重放时返回 event ID 和 event payload 的原始持久化字节，不采信
重试方重新提交的 `StateChange` 或重新计算的 ExecutionJob。

SQLite 当前 schema 是 v3。v1 的旧回执和 v2 的复合回执结构都在启动事务内一次性迁移到
v3：v1 回执缺少 actor 和 scope，只能进入一个保留的迁移身份；v2 增加 opaque aggregate
journal 表。状态、outbox 顺序和发布状态保持不变。迁移完成后只运行 v3，不存在 v1 全局
`requestId` 查询或 v2 无 journal 的第二条运行路径。迁移任一步失败时，版本号和建表变更
一起回滚，服务不会开放命令入口。

Delivery 的唯一 Control Plane 写入口是 `ControlPlane::commit_delivery_execution`。它先用
transaction-scoped journal adapter 生成不透明的 Delivery record，再把 record、canonical
Delivery snapshot、带 actor 和完整 scope 的回执、原始 ExecutionJob outbox 放入同一个
`ProductStateStorage` commit。数据库返回成功后才派发 receipt 中恢复出的原始 job；普通
`ControlPlane::commit` 拒绝 Delivery 命令，避免再次形成 journal 已成功而外层状态失败的
分离写入路径。

重复请求与 revision 冲突的具体结果继续使用 canonical HTTP 错误合同；存储 adapter 只
实现同一事务结果，不新增 adapter 专用业务语义。

## 启动顺序

Control Plane 固定按以下顺序启动：

1. `load-configuration`
2. `create-owned-temporary-root`
3. `open-storage`
4. `apply-pending-migrations`
5. `recover-committed-outbox`
6. `start-owned-services`
7. `accept-commands`

因此必须先完成迁移，再接收命令。迁移失败或发现不支持的数据库版本时，启动直接失败；
HTTP、WebSocket 和 ExecutionPort 入口不能进入可服务状态。

临时根目录必须带当前实例可验证的所有权标记。启动时只能清理所有权租约已经失效的旧
目录，不能按模糊路径或通配符删除任意用户目录。迁移失败时不开放命令入口，并继续关
闭已经打开的 storage、释放本实例的临时根目录，然后返回 `StartError`。

## 崩溃恢复

恢复只使用已经持久化的事实：

- 未提交事务由数据库完整丢弃；
- 迁移中断必须在开放服务前继续或回滚到一个完整版本；
- 只重放已经提交但尚未发布的 outbox 事件；
- 重放按持久化 outbox sequence 顺序进行，接收方按事件 ID 去重；
- 进程崩溃遗留的临时目录，只能在所有权租约明确失效后清理。

恢复不能从“最后一次内存回调已经运行”推断事件成功，也不能为了补事件重新执行一次
业务命令。这样重启不会制造第二次状态变更。

## 关闭顺序

Control Plane 固定按以下顺序关闭：

1. `stop-accepting-commands`
2. `stop-producing-new-outbox-events`
3. `wait-for-owned-command-work`
4. `flush-committed-outbox`
5. `close-event-publisher`
6. `close-storage`
7. `release-owned-temporary-root`

关闭先阻止新命令进入，再等待已经归 Control Plane 管理的命令结束，然后发送已提交的
outbox 事件。事件发送器关闭后才释放数据库连接；最后删除本实例拥有的临时目录。

关闭完成后不得留下仍在运行的全局后台任务。将来需要 dispatcher 或 scheduler 时，
每个任务都必须有 Control Plane 生命周期持有的句柄，并在 `shutdown` 返回前收到停止
信号和完成等待。进程级 `static` Runtime 或 `static` JoinHandle 不属于可接受的任务
所有权。

flush 失败也不能提前跳出清理。未发布的 outbox 保持可恢复，Control Plane 仍然关闭
event publisher、storage 并释放自有临时目录，最后返回 `ShutdownError`。这样调用方既
知道本次发布没有完成，也不会得到仍占用数据库和目录的半关闭进程。

## SQLite 与 PostgreSQL

本地实现使用 `SqliteStorage`，企业实现预留 `PostgresStorage`，两者都位于
`ProductStateStorage` 后面。PostgreSQL 是后续 adapter，不是阶段 2.1 已实现能力。

两个 adapter 必须保持相同的产品结果：

- 事务边界相同；
- revision 冲突结果相同；
- canonical state 和 outbox 的追加顺序相同；
- 崩溃后的 outbox 恢复相同；
- 迁移完成前都不开放服务；
- 关闭后连接和临时资源都已释放。

SQLite 的 WAL、busy timeout 或 PostgreSQL 的 isolation level 是 adapter 实现细节，
不得渗入 Delivery、Approval 或 Session 的公开合同。

## Rust 公共检查边界

阶段 2.1 冻结以下公共名字，让集成测试从 crate 外部验证行为。阶段 2.3.1 在这条边界上增加
Delivery 专用原子入口和 opaque aggregate journal primitive。Control Plane 的通用提交
入口只接受 canonical `CommandEnvelope + StateChange`；低层 `StateCommit` 和回执身份键
只属于 `winwincode-storage` 端口，不从 Control Plane 根模块导出：

生命周期入口是 `ControlPlane::start_local`、`ControlPlane::commit` 和
`ControlPlane::shutdown`；测试只能通过这些公开入口观察启动、提交和关闭结果。

```text
winwincode-control-plane
├─ ControlPlane
│  ├─ ControlPlane::start_local
│  ├─ ControlPlane::commit
│  ├─ ControlPlane::commit_delivery_execution
│  └─ ControlPlane::shutdown
├─ ControlPlaneConfig
├─ EventPublisher
├─ StateChange / CommitReceipt
├─ ShutdownReport
└─ StartError / CommitError / ShutdownError

winwincode-storage
├─ ReceiptActorKey / ReceiptScopeKey / ReceiptIdentity
├─ StateCommit
├─ AggregateJournalKey / AggregateJournalRecord
├─ AggregateJournalPublication / LoadedAggregateJournal
├─ ProductStateStorage
│  ├─ commit
│  ├─ load_state
│  ├─ load_journal
│  ├─ pending_events
│  ├─ mark_published
│  └─ close
└─ SqliteStorage::open
```

Rust 集成测试目标固定为
`crates/winwincode-control-plane/tests/lifecycle.rs`，并覆盖：

- 启动迁移完成前拒绝提交；
- 启动迁移失败时关闭 storage 并释放临时目录；
- 状态与 outbox 先提交，随后才发布；
- outbox 插入失败时状态一起回滚；
- 提交后的发布失败保留状态和 pending outbox，重启后再发送；
- 重启只重放已提交但未发布事件；
- 关闭先 flush outbox，再关闭 publisher 和 storage；
- 关闭时发布失败也继续关闭 storage 并释放临时目录；
- 关闭释放 SQLite 连接和临时目录。
- 相同 actor、完整 scope 和 requestId 只重放相同命令摘要；
- 不同 actor 或完整 scope 可以独立使用相同 requestId；
- JSON 对象键顺序不改变命令摘要；
- 非法 scope ID 在调用 storage 前失败；
- 重放的 event ID 来自持久化 outbox，而不是重试的 StateChange。

Node 门禁会通过 `cargo test --list` 核对固定测试名，并实际运行生命周期目标。阶段 2.3.1
还固定运行 `delivery_atomic_transaction.rs`，从 crate 外部验证 Delivery 原子提交、四个事务
成员的失败回滚、原始 job 重放、旁路拒绝，以及 dispatch/ack 失败后的重启补发。Rust crate
尚未出现时，这些检查不会被当成已经通过。

## 依赖与后台任务边界

Control Plane 不依赖 Codex Core。两个阶段 2.1 crate 都不得引入 `codex-*`、
`winwincode-codex`、`winwincode-kernel`、`winwincode-native` 或 N-API。实际 manifest
出现后，门禁通过 `cargo metadata` 检查它们和所有可达 workspace crate。

`winwincode-storage` 的产品依赖只能来自目标图允许的 `winwincode-domain`；
`winwincode-control-plane` 也只能引用目标图列出的 Control Plane 模块。数据库 driver、
序列化和临时目录库可以是外部实现依赖，但不能把 Worker 或 Codex 带入 Control Plane。
