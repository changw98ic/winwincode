# Client 控制面状态机

- 基础对应任务：`winwincode-9fu.2`（[CLIENT-000.2] 连接四层状态机规范文档）
- 同批 Phase 0 产出：ADR `multi-user-client-access-and-occupancy.md`、协议合同
  `client-control-port-v1.md`（由其他任务冻结，本文不重复其内容）
- 机器可读规则：暂无；状态机冻结后如需门禁测试，再生成配套 `rules.json`

这是一份**状态机冻结合同，不是实现完成声明**。它把多用户共享 Client 计划中的全部
控制面状态枚举和合法迁移固定为唯一权威版本。后续 Schema、Rust/TS 合同生成和各
Phase 实现只能实现这里列出的状态与迁移，不得另发明状态、别名或捷径迁移。

除非特别说明：

- 每个领域对象的状态值都是单值枚举；
- 状态变更必须与 revision 校验、领域 journal 和 outbox 在同一事务提交；
- 消息名引用 ClientControlPort v1 的消息种类（`client.*`）；
- 表中"ACK / fencing"列说明该迁移是否需要 Device Client ACK，或命令是否必须携带
  occupancy fencing token 才能在 Device Client 本地通过校验。

## 0. 权威所有者

| 事实 | 权威所有者 |
| --- | --- |
| 用户、Client 授权、Client 占用 | Control Plane |
| Client 在线、平台、版本、容量 | Device Client 上报，Control Plane 持久投影 |
| Worker 进程与本地 PID、本地绝对路径 | Device Client 本地数据库 |
| Candidate 本地 Git ref | Device Client / Worker |
| WorkerSession、Lease、Fencing、Job | 现有 Worker / Control Plane 合同 |
| Candidate、Evidence、Verdict 产品结论 | Control Plane |

占用采用"Control Plane 权威 + Device Client 执行"：Control Plane 原子创建占用
Lease；Device Client ACK 对应 fencing token 后状态才进入 `occupied`；Device Client
拒绝任何与本地占用镜像不匹配的命令；stale Lease 即使在 Server 上被重放，也不能在
Client 落地。

## 1. ClientNodePresence

机器级常驻状态，不属于任何 Browser Session。设备在线不等于有用户连接。

```text
pending_enrollment
online
degraded
offline
locked
revoked
```

### 合法迁移

| 当前状态 | 目标状态 | 触发事件 | 触发者 / 权威所有者 | ACK / fencing 要求 |
| --- | --- | --- | --- | --- |
| —（初始） | `pending_enrollment` | Device Client 首次注册，提交 `client.enroll` | Device Client 发起；Control Plane 创建登记记录 | 无 |
| `pending_enrollment` | `online` | 登记被接受（`client.enrollment_accepted`）且首次 hello / 心跳成功 | Control Plane 判定；Device Client 上报 | 无 |
| `pending_enrollment` | `revoked` | 登记被拒绝、过期或设备凭据被吊销 | Control Plane | 无 |
| `online` | `offline` | 心跳 / 交换在超时窗口内未到达 | Control Plane 持久投影（时间判定） | 无 |
| `online` | `degraded` | Device Client 重启后报告本地 Worker / 占用镜像未完成对账 | Device Client 上报，Control Plane 投影 | 无 |
| `online` | `locked` | 锁定：本地 UI / CLI 操作或 `client.client_lock` | 本地操作者或 Control Plane 下发；Device Client 本地强制执行 | 无（非占用命令，不涉及 fencing） |
| `offline` | `online` | 以同一 identity / generation 重连且无未完成对账 | Device Client 上报 | 无 |
| `offline` | `degraded` | 重连成功但本地对账未完成 | Device Client 上报 | 无 |
| `degraded` | `online` | `client.worker.reconcile` 被接受，对账完成 | Device Client 上报；Control Plane 判定 | 必须有被接受的对账结果上报（等价 ACK） |
| `degraded` | `offline` | 对账完成前再次失联 | Control Plane 投影 | 无 |
| `locked` | `online` | 本地解锁 | 本地操作者 | 无 |
| `online` / `offline` | `revoked` | 管理员撤销设备或凭据被吊销 | Control Plane | 无 |

### 终态与说明

- `revoked` 是唯一终态。计划未定义 revoked 设备用原身份恢复的路径；重新使用需要走
  新的登记流程。
- 其余状态都是可持续或可恢复状态：`degraded` 只表达"本地状态未完成对账"，必须以
  `client.worker.reconcile` 结束；`offline` 只表达连接不可达，不表达本地任务结局。
- `acceptingConnections`（禁止新连接）是 ClientNode 上的独立布尔开关，不是 presence
  状态，不进入本状态机。

## 2. ClientConnectCode

```text
active
consumed
expired
revoked
```

### 合法迁移

| 当前状态 | 目标状态 | 触发事件 | 触发者 / 权威所有者 | ACK / fencing 要求 |
| --- | --- | --- | --- | --- |
| —（初始） | `active` | Device Client 生成一次性 code 并发布摘要（`client.connect_code.published`） | Device Client 生成；Control Plane 只保存 HMAC / 摘要 | 无 |
| `active` | `consumed` | 校验通过、Device Client 确认该 code generation 仍有效并 ACK challenge 后，Control Plane 原子创建 ClientAccessGrant | Control Plane 判定 | 必须：Device Client challenge ACK（`client.access.challenge_ack`） |
| `active` | `expired` | `expiresAt` 到期（默认 2 分钟） | Control Plane（时间判定） | 无 |
| `active` | `revoked` | Client 刷新连接码或主动作废该 code | Device Client 本地操作并发布更新 | 无 |

### 终态与说明

- `consumed`、`expired`、`revoked` 都是终态，不可逆。
- code 一次性：`consumed` 后重放一律拒绝。
- 错误尝试由用户级、IP 级和 Client 级限流处理；限流是策略执行，不改变 code 状态，
  不进入本状态机。
- 最终授权前必须收到在线 Client 对 challenge 的 ACK；Client 离线时校验直接失败。

## 3. ClientAccessGrant

```text
active
revoked
expired
```

### 合法迁移

| 当前状态 | 目标状态 | 触发事件 | 触发者 / 权威所有者 | ACK / fencing 要求 |
| --- | --- | --- | --- | --- |
| —（初始） | `active` | connect code 流程完成、管理员创建或本地确认（`grantSource` 三选一） | Control Plane 原子创建 | `grantSource=connect_code` 时必须先有 Device Client challenge ACK |
| `active` | `revoked` | Owner 或持有 manage / share 权限的用户撤销访问 | Control Plane | 无；撤销立即生效，不等待 Device Client |
| `active` | `expired` | `expiresAt` 到期（temporary grant） | Control Plane（时间判定） | 无 |

### 终态与说明

- `revoked` 和 `expired` 是终态；`active` 是唯一可使用状态。
- grant 只表达"允许使用"，不产生占用。
- `trusted` 只免连接码，不免占用检查、Repository ACL 和 Client 在线检查。

## 4. ClientOccupancyLease

```text
available
reserving
occupied
draining
recovery_pending
released
expired
```

`available` 不是一条活动 Lease 记录的状态，而是"该 clientNodeId 当前没有活动
Lease"的投影（从未创建或历史 Lease 已终态）。活动 Lease 记录从 `reserving` 开始，
数据库必须保证一个 `clientNodeId` 最多一个活动 Lease。占用属于 `holderUserId`，不
属于任何浏览器 cookie。

### 合法迁移

| 当前状态 | 目标状态 | 触发事件 | 触发者 / 权威所有者 | ACK / fencing 要求 |
| --- | --- | --- | --- | --- |
| 无活动 Lease（`available`） | `reserving` | 用户申请占用；Control Plane 原子检查：Browser Session 有效、grant 有 use、Client 在线且未锁定、无活动 Lease、至少一个可用 Session 槽位；通过后创建 Lease 并铸造新 fencing token | 占用者用户（任一有效 Browser Session）发起；Control Plane 原子判定 | 无需 ACK；新 fencing token 在此生成 |
| `reserving` | `occupied` | Device Client 持久保存占用镜像并 ACK（`client.occupancy.ack`） | Control Plane 记录；Device Client ACK | 必须：`client.occupancy.ack` 匹配 Lease 与 fencing token |
| `reserving` | `released` | offer 未在窗口内 ACK、Device Client 拒绝（`client.occupancy.rejected`）或申请人撤销 | Control Plane | 无需 ACK（`rejected` 本身是 Client 上报） |
| `occupied` | `draining` | 释放请求但存在活动任务："完成当前任务后释放"或"取消全部任务并释放"（后者需明确确认） | 占用者用户（任一有效 Browser Session）或管理员 force-clean | 命令携带 Lease + fencing token；Client 此后拒绝新 launch |
| `occupied` | `released` | 释放请求且无活动任务 | 占用者用户或管理员 | 命令携带 Lease + fencing token（`client.occupancy.release`） |
| `draining` | `released` | 全部活动 WorkerSession 达到终态 | Control Plane 自动判定 | 无需 ACK |
| `occupied` | `recovery_pending` | 占用期间 Client 心跳丢失 | Control Plane 投影 | 无需 ACK；本地任务状态暂不可确认 |
| `draining` | `recovery_pending` | draining 期间 Client 掉线 | Control Plane 投影 | 无需 ACK；按与 `occupied` 相同的掉线规则处理（见开放点） |
| `recovery_pending` | `occupied` | 恢复窗口内以同一 identity / generation 重连，对账报告本地 Worker 仍在 | Device Client `client.worker.reconcile`；Control Plane 判定 | 必须：对账结果被接受（等价 ACK）；fencing token 不变，因为未发生新占用 |
| `recovery_pending` | `draining` | 对账报告任务已结束或进入收尾 | 同上 | 必须：对账结果被接受；fencing token 不变 |
| `recovery_pending` | `released` | 超过 `recoveryDeadlineAt` 后由管理员或原占用者执行安全清理 | 管理员或原占用者 | 显式安全清理；绝不自动把未知本地任务转交新用户 |
| `occupied` | `expired` | 无活动任务且达到 idle policy（`idleExpiresAt`） | Control Plane（时间判定） | 无需 ACK |

### 终态与说明

- `released` 与 `expired` 是 Lease 记录的终态；记录保留为审计历史。之后 clientNodeId
  的占用投影回到 `available`，下一次占用必须创建新 Lease 并铸造更高的 fencing token。
- `reserving` 不是稳定状态：它要么被 ACK 提升，要么终结为 `released`。
- `recovery_pending` 没有自动终态：超过恢复期限也不自动转交，必须管理员 / 原占用者
  执行安全清理。

## 5. WorkerLaunchGrant

```text
issued
consumed
revoked
expired
```

### 合法迁移

| 当前状态 | 目标状态 | 触发事件 | 触发者 / 权威所有者 | ACK / fencing 要求 |
| --- | --- | --- | --- | --- |
| —（初始） | `issued` | 任务提交并通过 OccupancyLease、RepositoryAccessGrant 和容量校验后，创建 ExecutionReservation 与 WorkerLaunchGrant | Control Plane | Grant 绑定 `occupancyLeaseId` + `occupancyFencingToken` 与完整身份（§17.2）；无需 Client ACK |
| `issued` | `consumed` | Worker 以匹配的完整身份通过现有 ExecutionPort 注册成功 | Server 校验注册身份；Device Client 已完成本地 fencing 校验并启动进程 | 必须：`client.worker.launch` 经 Device Client fencing 校验；consume 以 Worker 注册事实结算，duplicate launch 幂等 |
| `issued` | `revoked` | 启动前取消任务或发送 `client.worker.stop` | 占用者用户或 Control Plane | `client.worker.stop` 必须匹配当前 fencing token |
| `issued` | `expired` | `expiresAt` 前未消费 | Control Plane（时间判定） | 无 |

### 终态与说明

- `consumed`、`revoked`、`expired` 都是终态；grant 不复用。
- 任何绑定字段（clientNodeId、clientInstanceId、Lease、fencing token、repository、
  session / worker 身份、expiry）不一致均拒绝。
- retry 不复用 stale Worker / Lease；重试需要新的 WorkerLaunchGrant。
- 已结算的 launch 不因 Server 重启或重放而重复执行。

## 6. LocalCandidateReceipt

```text
retained
branch_created
applied
discarded
failed
```

### 合法迁移

| 当前状态 | 目标状态 | 触发事件 | 触发者 / 权威所有者 | ACK / fencing 要求 |
| --- | --- | --- | --- | --- |
| —（初始） | `retained` | 验证 Workspace、创建 Candidate commit，并在清理 Worktree 前创建 `refs/winwincode/candidates/<candidate-id>`，上报 `client.candidate.retained` | Worker 执行；Device Client 拥有本地 ref | 无需 fencing；Candidate 产出属于运行中任务自身的上报 |
| `retained` | `branch_created` | 用户请求"创建本地分支"，Device Client 创建 `winwincode/<task-slug>-<short-id>` | 占用者用户发起；Device Client 执行并上报 `client.candidate.apply_result` | 必须：有效占用 Lease + fencing token |
| `retained` | `applied` | 用户请求"应用到目标分支"且全部预检通过，在隔离 integration worktree 中执行 | 同上 | 必须：有效 Lease + fencing token，且 `expectedHead` 匹配 |
| `retained` | `discarded` | 用户请求丢弃 Candidate | 同上 | 必须：有效 Lease + fencing token |
| `branch_created` | `applied` | candidate ref 仍存在时请求应用到目标分支 | 同上 | 同 `retained → applied` |
| `branch_created` | `discarded` | 请求丢弃 | 同上 | 同 `retained → discarded` |
| `retained` / `branch_created` | `failed` | 应用或丢弃操作失败：预检不通过（结果码 `candidate_missing`、`base_stale`、`working_tree_dirty`、`merge_conflict`、`permission_denied`）或执行错误 | Device Client 上报 `client.candidate.apply_result` | 命令本身必须带有效 Lease + fencing token；失败结果写入 LocalApplyReceipt |
| `failed` | `applied` / `branch_created` / `discarded` | 重试操作成功（每次操作产生新的 LocalApplyReceipt） | 同上 | 同对应成功迁移 |

### 终态与说明

- `applied` 与 `discarded` 是候选生命周期的终态。
- `retained` 和 `branch_created` 是可持续的中间状态：candidate ref 继续存在，可继续
  应用或丢弃。
- `failed` 不是终态，支持重试与审计；每次重试写新的 receipt，不改写旧 receipt。
- 稳定 ref 必须在 Worktree 清理前创建，保证 Candidate 不随临时 Worktree 丢失。
- Candidate 产品结论由 Control Plane 拥有；本地 ref 由 Device Client / Worker 拥有。

## 7. RepositoryBinding 本地状态投影

```text
available
dirty
unavailable
moved
invalid_git
permission_denied
scan_failed
```

这不是事务状态机，而是 Device Client 本地扫描产生的状态投影：每次重新扫描——注册
时、收到 `client.repository.rescan`、以及每次 Worker Launch 前的强制重验证——都会重
新计算。任意状态之间都可在重新验证后迁移，不存在终态。binding 移除
（`client.repository.removed`）不属于本状态机。绝对路径只存在于 Device Client 本地。

### 状态判定与门禁效应

| 状态 | 判定来源（注册 / 重验证检查链） | 门禁效应 |
| --- | --- | --- |
| `available` | canonicalize 通过、目录存在可读、Git common directory 有效、工作树 clean | 允许 launch 与 apply |
| `dirty` | 同上，但工作树有未提交修改 | 允许 launch（隔离 Worktree）；apply 到目标分支 fail closed（`working_tree_dirty`） |
| `unavailable` | 目录不存在或不可读 | fail closed：拒绝 launch 与 apply |
| `moved` | canonical path 不再解析到原目录（含软链接替换） | fail closed |
| `invalid_git` | Git 检查失败且用户未确认初始化 | fail closed |
| `permission_denied` | 操作系统权限拒绝 | fail closed |
| `scan_failed` | 扫描自身失败，原因不确定 | fail closed |

权威所有者：Device Client 本地计算；Control Plane 只持久化安全元数据投影（commit、
branch、dirty 状态、binding 身份），不保存绝对路径。仅 `available` 与 `dirty` 允许
launch；每次 launch 前必须重新 canonicalize 和验证，不依赖旧扫描结果。

## 8. LocalApplyReceipt 应用结果

一次"创建本地分支 / 应用到目标分支 / 丢弃"操作产生一条 `LocalApplyReceipt`，记录
strategy、expectedHead、resultingCommit、conflictArtifactRef 和结果码。结果码集合：

| 结果码 | 含义 | 对应 receipt `state` |
| --- | --- | --- |
| `retained` | 操作后候选保持保留，未创建分支也未应用 | `retained` |
| `branch_created` | 本地分支创建成功 | `branch_created` |
| `applied` | 已应用到目标分支 | `applied` |
| `base_stale` | 目标 HEAD ≠ `expectedHead`，fail closed | `failed` |
| `working_tree_dirty` | 目标工作树不满足策略，fail closed | `failed` |
| `merge_conflict` | 隔离 integration worktree 中发生冲突；冲突产物记入 `conflictArtifactRef`，不写入用户当前工作区 | `failed` |
| `candidate_missing` | candidate ref 不存在 | `failed` |
| `permission_denied` | 当前用户缺少 repo use / manage 权限 | `failed` |
| `discarded` | 候选已丢弃 | `discarded` |
| `failed` | 其他执行失败 | `failed` |

### 迁移与终态说明

- 结果只落一次，不支持就地改写；重试产生新 receipt，支持重试与审计。
- 应用前必须校验：candidate ref 仍存在、current target HEAD == expectedHead、目标
  工作树状态满足策略、占用 Lease 和 fencing token 有效、当前用户有 repo
  manage / use 权限。任何一项不满足即 fail closed，不产生部分应用。
- `applied` 与 `discarded` 终结候选生命周期；失败结果码都可重试。

## 9. 跨对象不变量

以下不变量横跨多个领域对象，任何实现、重放或恢复路径都不得破坏。

### 9.1 一个 clientNodeId 最多一个活动 Lease

Control Plane 必须在数据库层保证一个 `clientNodeId` 最多一条非终态占用 Lease。
claim 是原子操作：两用户并发申请同一 Client 时只有 `reserving` 创建成功者继续，另
一个得到明确失败。`available` 只是无活动 Lease 的投影，不是可被两条 Lease 同时引用
的状态。

### 9.2 Fencing token 单调递增，旧 token 永远拒绝

每次新占用（新 Lease）铸造严格更高的 fencing token。Device Client 对 Worker
launch、Worker stop、Candidate apply 和 Repository mutation 强制校验 Leases 与
token：旧 token 永远拒绝，包括网络重放和 Server 重启后的重放。stale Lease 即使在
Server 上被重放，也不能在 Client 落地。恢复对账复用原 Lease 的 token；只有新占用
才铸造新 token。

### 9.3 未 ACK 不进入 occupied

`reserving → occupied` 的唯一触发是收到匹配 Lease 与 fencing token 的
`client.occupancy.ack`，且 Device Client 已把占用镜像持久化到本地。ACK 之前 Web 不
允许该用户查看获授权仓库或创建任务。offer 超时、被拒绝或申请人撤销时，Lease 终结
为 `released`，不进入 `occupied`。

### 9.4 Client 离线不允许抢占，必须先对账

占用期间（`occupied` 或 `draining`）Client 掉线时，Lease 进入 `recovery_pending`：
不允许其他用户抢占，不允许新任务，当前任务标记为"运行事实暂不可确认"，UI 不得伪
装 Running 或 Failed。Client 必须在恢复窗口内以同一 identity / generation 重连并提
交 `client.worker.reconcile`，对账成功后恢复 `occupied` 或进入 `draining`。超过恢复
期限也不自动把未知本地任务交给新用户；只有管理员 / 原占用者执行安全清理后才回到
`released`。

### 9.5 Browser 关闭不释放占用

占用绑定 `holderUserId`，不属于浏览器 cookie 或标签页。关闭浏览器、单个标签页关闭
或 Browser Session 过期都不释放占用、不终止本地任务；用户重新登录后按 `userId` 恢
复控制，其任意有效 Browser Session 都可控制同一占用。占用只能通过显式释放、有活动
任务时的 drain 流程、或无活动任务时达到 idle policy 的自动释放（`expired`）结束。

## 10. 计划未定义、留给后续 ADR 的开放点

以下三点在实施计划中没有明确规定；本文采用了最保守的读法，最终语义由 ADR
`multi-user-client-access-and-occupancy.md` 确认：

1. `locked` 与 `offline` 的叠加投影：presence 是单值枚举，锁定中的设备失联时展示
   `locked` 还是 `offline` 未定义。
2. `reserving` 未被 ACK 的结束状态命名：计划未明确；本文记为 `released`，用
   `releaseReason` 区分 ack 超时、Client 拒绝与申请人撤销。
3. `draining` 期间掉线是否进入 `recovery_pending`：计划的掉线规则只写了
   `occupied`；本文按同一条掉线规则处理。
