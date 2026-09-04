# ClientControlPort v1 合同

`ClientControlPort` 是 WinWinCode Server 与 Device Client 之间唯一的机器级控制边界。
机器可读的合同由同一 Phase 冻结的
[`schema/winwincode/v1/client-control.schema.json`](../../schema/winwincode/v1/client-control.schema.json)
提供。本文件解释该合同的传输与状态语义，不建立第二份类型定义。

消息 `kind` 字符串以本文两张目录表为准，与实施计划 §9 逐条一致；新增、删除或改名
都必须先修订本合同。

## 为什么独立于 ExecutionPort

目录、设备、占用和 Worker 启动是机器级控制；ExecutionPort 是任务执行协议。
ClientControlPort 只承载 ClientNode 注册、presence、连接授权、占用 Lease、
Repository Registry 投影、Worker 进程生命周期和 Candidate 本地落地控制。
ExecutionPort 继续独占 Job、Attempt、Execution Lease、runtime 事件、产物、模型流、
输入与审批。两者混合会产生两个后果：

- Device Client 必须理解并有能力执行任务消息，变成一个万能 Worker；
- 设备注册与占用的频繁变化进入执行合同，破坏 ExecutionPort 已冻结的稳定边界。

两个端口只通过身份引用关联（`clientNodeId`、`occupancyLeaseId`、`workerSessionId`），
不共享消息 union。Worker 继续直接使用现有 Remote ExecutionPort 交换执行流量；
Device Client 不代理 runtime、artifact 或 model frame，避免再造一层吞吐瓶颈。

## 所有权边界

- Control Plane 拥有 ClientNode Registry、ClientAccessGrant、Connect Code 状态、
  ClientOccupancyLease 与 fencing token 分配、RepositoryBinding 投影、
  RepositoryAccessGrant、WorkerLaunchGrant，以及每个 Client 的命令 outbox 与 cursor。
- Device Client 拥有本地路径映射、worker 进程注册、occupancy mirror、candidate
  本地 ref 与 LocalApplyReceipt、`client_outbox` 和 `client_inbox_cursor`。
- Worker 拥有执行事实，经 ExecutionPort 上报。本端口不携带 `runtime.event`、
  `artifact.*`、`model.*`、`input.*`、`approval.*` 或 `job.outcome`。

消息只引用公开 ID 和投影。Server 不提交绝对路径，路径映射永不上传；消息不包含
长期 Provider Credential，也不包含 Codex 的 Turn、Plan 或 Agent 内部对象。

## 传输：POST /internal/v1/client/exchange

V1 复用现有 Remote ExecutionPort 的设计经验：

- Device Client 主动发起，每次 POST 是一次 exchange；
- HTTPS，仅接受 Device Credential 认证；
- bounded frame；
- 双向 sequence 与 acknowledgement；
- 断线后按 sequence 重放；
- 双侧 durable outbox；
- Server 重启与网络抖动后可恢复。

V1 没有入站端口，也没有 P2P：Server 永远不直接连接 Device Client，所有下行命令都
等待 Client 的下一次 exchange 投递。

一次 exchange 的形状：

- 请求携带一批有界的 Client → Server frame（自 Client 已确认位置起），以及 Client 已
  连续处理的 Server → Client `ackSequence`；
- 响应携带对请求批的 `ackSequence` 与可选 `replayFromSequence`，以及一批自 Client
  已确认位置起的有界下行 frame；
- 未投递完的剩余 frame 由后续 exchange 继续拉取。

frame 数量与字节上限属于 Adapter 配置，不进入消息合同。认证材料不进入 URL、
query 或任何 frame payload；Browser Session Cookie、Connect Code 和 Worker Session
Credential 在该端点一律无效并进入审计。

sequence 按 `clientNodeId` 分流：Client → Server 序列由 Client 的 durable outbox
分配，Server → Client 序列由 Server 按 Client 分配。两个流各自从 1 开始连续递增。
`ackSequence` 表示接收方已连续接受的最大 sequence，而不是"见过的最大值"；状态为
`gap` 时必须返回 `replayFromSequence`。发送方在持久化 `ackSequence` 后可以压缩已确认
frame，但不得缺失任何未确认的连续 frame；缺失即按损坏状态拒绝恢复。

## Envelope

每个 frame 使用同一份 Envelope：

| 字段 | 语义 |
| --- | --- |
| `schemaVersion` | 合同版本；本合同为 v1，不匹配的版本整批拒绝 |
| `messageId` | 发送方分配的 frame 身份；重放与去重按 messageId、sequence 和 payload digest 判定 |
| `clientNodeId` | 稳定公开的 Client 身份；可查找，不可作为凭据 |
| `clientInstanceId` | 一次 Device Client 进程启动；每次重启生成新值 |
| `sequence` | 发送方流内单调连续位置，先持久化再发送 |
| `occurredAt` | 发送方时钟观察值，RFC 3339 UTC |
| `kind` | 下方目录表中的精确字符串 |
| `payload` | kind 专有对象 |

所有命令必须带：

```text
expectedRevision
idempotencyKey
```

`expectedRevision` 是命令所基于的权威聚合 revision：Client → Server 命令针对 Server
侧聚合（ClientNode、Lease、Binding 投影等），Server → Client 命令针对 Server 计算
命令所依据的 Client 已确认镜像 revision。接收方校验失败时拒绝命令并返回当前
revision，发送方重新计算后再试。`idempotencyKey` 由发送方生成；同一 key 对应不同
payload 是冲突，不覆盖已接受的结果。

涉及占用或任务执行的命令还必须带：

```text
occupancyLeaseId
occupancyFencingToken
```

`occupancyFencingToken` 使用十进制字符串，避免跨 Rust、JavaScript 和数据库时丢失
64 位整数精度；每次新占用生成更高的 token。

## Client → Server 消息

强制字段标记：`—` 纯事实；`C` 命令（`expectedRevision` + `idempotencyKey`）；
`C + L` 命令再加占用盖章（`occupancyLeaseId` + `occupancyFencingToken`）；
`C + L（活动占用时）` 表示存在活动 Lease 时必须盖章；`L` 绑定占用的事实报告。

| kind | 方向 | payload 概要 | 发送/处理时机 | 强制字段 |
| --- | --- | --- | --- | --- |
| `client.enroll` | Client → Server | 设备名、凭据请求材料、clientInstanceId、协议版本 | Device 首次启动且本地无已接受身份时发送；Server 以 `client.enrollment_accepted` 回应 | `C` |
| `client.hello` | Client → Server | clientInstanceId、软件版本、inbox cursor、outbox 起点、待对账标记 | 进程启动以及每次断线恢复后的第一个交换中发送；Server 校验后继续两个流并把旧实例标记为被取代 | `C` |
| `client.heartbeat` | Client → Server | presence、容量（`maxConcurrentWorkerSessions`、`runningWorkerSessions`、`reservedWorkerSessions`、draining）、lastObservedAt | 空闲期按固定间隔发送；Server 不得从 heartbeat 隐式派发命令 | `—` |
| `client.connect_code.published` | Client → Server | connectCodeId、codeDigest、issuedByInstanceId、expiresAt、remainingAttempts、generation | 用户在设备上生成或刷新动态连接码后发送；Server 只保存摘要 | `C` |
| `client.access.challenge_ack` | Client → Server | challengeId、connectCodeId、generation、accepted 或拒绝原因 | 回应 `client.access.challenge`：确认该 code generation 本地仍有效；Server 收到后原子创建 ClientAccessGrant | `C` |
| `client.occupancy.ack` | Client → Server | occupancyLeaseId、occupancyFencingToken、mirrorRevision | 持久保存占用镜像后回应 `client.occupancy.offer`；Server 收到后 Lease 才进入 occupied | `C + L` |
| `client.occupancy.rejected` | Client → Server | occupancyLeaseId、occupancyFencingToken、reason（本地锁定、容量、存储失败等） | 无法接受 offer 时回应；Lease 不进入 occupied，Server 回滚 reserving | `C + L` |
| `client.repository.upsert` | Client → Server | repositoryBindingId、displayName、repositoryKind、defaultBranch、headCommit、dirtyState、availability、repositoryFingerprint、revision；无绝对路径 | 本地注册仓库或绑定元数据变化后上报安全投影 | `C + L（活动占用时）` |
| `client.repository.removed` | Client → Server | repositoryBindingId、revision、reason | 本地移除绑定后上报；Server 作废对应投影与授权入口 | `C + L（活动占用时）` |
| `client.repository.status` | Client → Server | repositoryBindingId、availability（available、dirty、unavailable、moved、invalid_git、permission_denied、scan_failed）、headCommit、dirtyState、lastScannedAt | 响应 `client.repository.rescan` 或本地检测到状态变化；每次 Worker Launch 前仍需重新 canonicalize，不依赖旧扫描结果 | `C + L（活动占用时）` |
| `client.worker.launch_ack` | Client → Server | workerLaunchGrantId、workerSessionId、workerInstanceId、accepted 与拒绝原因（fence、repo、容量、本地状态） | 回应 `client.worker.launch`：接受表示本地 launch intent 已持久化并已 spawn 子进程；拒绝时 grant 不被消费 | `C + L` |
| `client.worker.state` | Client → Server | workerSessionId、进程 state、exit 摘要、容量更新、lastObservedAt | lease 绑定的 Worker 进程状态迁移时发送，并随 heartbeat 汇总；不承载 ExecutionPort 执行事实 | `L` |
| `client.worker.reconcile` | Client → Server | 每个 pending intent 与注册条目的 workerSessionId、对账结果（still_running、terminal、missing、unknown）、pending launch/apply intent 列表 | Device Client 重启扫描本地状态后发送；Server 接受前 presence 保持 degraded、occupancy 保持 recovery_pending | `C + L` |
| `client.candidate.retained` | Client → Server | candidateRef、repositoryBindingId、candidateCommit、localRefName、diff 摘要与 Evidence 引用、receipt revision | 冻结完成、稳定 candidate ref 创建并清理 Worktree 前发送；相同身份重发幂等 | `C + L` |
| `client.candidate.apply_result` | Client → Server | localApplyReceiptId、candidateRef、repositoryBindingId、targetBranch、expectedHead、strategy、result、resultingCommit、conflictArtifactRef、revision | 回应 `client.candidate.apply`；本地 receipt 持久化后发送，支持重试与审计 | `C + L` |
| `client.command_ack` | Client → Server | 被确认命令的 messageId、status（applied、rejected）、reason、生效 revision | 确认无专用 ack 的 Server → Client 命令（release、force_fence、rescan、client_lock、credential_rotate）；确认涉及占用的命令时必须回显盖章 | `C` |

## Server → Client 消息

| kind | 方向 | payload 概要 | 发送/处理时机 | 强制字段 |
| --- | --- | --- | --- | --- |
| `client.enrollment_accepted` | Server → Client | clientNodeId、Device Credential 材料与指纹、server profile、下行流起点 | 回应 `client.enroll`；此后 Client 以该身份交换 | `C` |
| `client.access.challenge` | Server → Client | challengeId、connectCodeId、申请用户、expiresAt | Web 用户提交 Client ID 与连接码且服务端校验、限流通过后发送；等待 `client.access.challenge_ack`，未 ACK 不创建 grant | `C` |
| `client.occupancy.offer` | Server → Client | occupancyLeaseId、新 occupancyFencingToken、holderUserId、claimRequestId、idleExpiresAt | 原子检查通过并创建 reserving Lease 后发送；Client 持久化占用镜像后回 `client.occupancy.ack`。offer 本身携带的新 Lease 与 token 即被 ACK 回显的盖章值 | `C + L` |
| `client.occupancy.release` | Server → Client | occupancyLeaseId、occupancyFencingToken、mode（release、drain、cancel_and_release）、reason | 占用者释放、drain 完成或取消全部任务并释放时发送；Client 停止接受新 WorkerSession，cancel 模式下停止现有 worker | `C + L` |
| `client.occupancy.force_fence` | Server → Client | occupancyLeaseId、更高 occupancyFencingToken、reason、要求的本地清理动作 | 管理员或原占用者安全清理路径发送；Client 以新 token 覆盖镜像并立即拒绝一切旧 token 命令 | `C + L` |
| `client.repository.rescan` | Server → Client | repositoryBindingId（或全部绑定）、reason | 投影过期或占用者请求刷新时发送；Client 重新 canonicalize 并回报 `client.repository.status` | `C + L（活动占用时）` |
| `client.worker.launch` | Server → Client | WorkerLaunchGrant 全部身份字段（workerLaunchGrantId、workerSessionId、workerId、workerInstanceId、repositoryBindingId、productSessionId、stageRunId、userId、occupancyLeaseId、occupancyFencingToken、credentialDigest、expiresAt）；不含绝对路径 | Scheduler 完成 durable ExecutionReservation 后发送；Client 校验 fencing、repo、容量与本地状态后写入 launch intent 并 spawn `winwincode-worker --managed-session` | `C + L` |
| `client.worker.stop` | Server → Client | workerSessionId、occupancyLeaseId、occupancyFencingToken、mode、reason | 占用者或管理员停止任务、drain 取消时发送；Client 校验 fencing 后停止进程，回报 `client.worker.state` 与 `client.command_ack` | `C + L` |
| `client.candidate.apply` | Server → Client | candidateRef、repositoryBindingId、targetBranch、expectedHead、strategy（create_branch、fast_forward、cherry_pick、merge）、occupancyLeaseId、occupancyFencingToken | 占用者从 Web 发起应用时发送；Client 在隔离 integration worktree 中执行校验与应用，回 `client.candidate.apply_result` | `C + L` |
| `client.client_lock` | Server → Client | locked（true、false）、reason、actorUserId | 管理员或本地用户锁定、解锁设备时发送；锁定后拒绝新连接与占用申请 | `C` |
| `client.credential_rotate` | Server → Client | rotationId、新 Device Credential 材料、新凭据生效时间与旧凭据吊销时间 | 凭据泄露或策略到期时发送；Client 持久化新凭据后回 `client.command_ack`，此后交换全部使用新凭据 | `C` |

每条 Server → Client 命令都要求显式确认：有专用 ack 的用专用 ack，其余用
`client.command_ack`。未确认的命令由 Server 保留在命令 outbox 中随后续 exchange 重放；
Client 依靠 `idempotencyKey` 保证重放不产生第二次本地动作。

## Fencing 强制校验点

Device Client 持久保存占用镜像（occupancyLeaseId、occupancyFencingToken、
holderUserId、mirrorRevision）。只有 `client.occupancy.offer` 和
`client.occupancy.force_fence` 能推进本地镜像；镜像更新后，所有基于旧 token 的未处理
命令立即失效。每次新占用生成更高的 fencing token，旧 token 永远拒绝，避免网络重放
让前任占用者复活。

Device Client 对以下命令强制校验（§12.6）：

| 校验点 | 命令 | 本地校验 | 失败动作 |
| --- | --- | --- | --- |
| Worker launch | `client.worker.launch` | 盖章与镜像完全一致；grant 绑定当前 clientInstanceId；RepositoryBinding 重新 canonicalize 并满足路径与软链接规则；本地容量可用 | `client.worker.launch_ack`（accepted=false），不写 launch intent，不 spawn |
| Worker stop | `client.worker.stop` | 盖章与镜像完全一致 | `client.command_ack`（rejected，stale fencing token），进程继续 |
| Candidate apply | `client.candidate.apply` | 盖章与镜像一致；candidate ref 仍存在；目标 HEAD 等于 expectedHead；目标工作树满足策略；用户有 repo 权限 | `client.candidate.apply_result`（failed，stale fencing token），写入 LocalApplyReceipt 供审计 |
| Repository mutation | `client.repository.upsert`、`client.repository.removed` 及占用触发的本地变更 | 存在活动占用时，变更必须基于当前盖章或来自持有当前盖章的指示 | 变更不生效，`client.command_ack`（rejected）；本地 binding 保持不变 |

盖章一致指 occupancyLeaseId 与 occupancyFencingToken 都与本地镜像完全一致；更低或
不匹配的 token 一律拒绝。Control Plane 对每条带盖章的 Client → Server 消息同样校验
Lease 与 token 是否为当前权威值，旧 token 不产生任何状态变化。

## 重试与恢复的固定结果

| 情况 | 合同结果 | 状态变化 |
| --- | --- | --- |
| 相同 messageId、sequence 和 payload digest 重放 | `duplicate` | 确认原 frame，不重复执行 |
| 相同 idempotencyKey、不同 payload | `rejected_conflict` | 不覆盖已接受的数据 |
| 收到的 sequence 大于 `ackSequence + 1` | `gap` | 保持原 `ackSequence`，返回 `replayFromSequence` |
| 断网恢复，同一 clientInstanceId 重新 exchange | `replay_required` | 双方按各自 cursor 重放未确认 frame |
| Device Client 重启产生新 clientInstanceId | `reacquire_required` | 旧实例标记为被取代，旧实例身份的命令与 grant 全部拒绝；本地扫描后必须先发送 `client.worker.reconcile` |
| Server 重启 | `resume` | 恢复 User Session、ClientNode、Lease、Grant 与双向 cursor；Client 重新 exchange，已结算的 launch/apply 不重复执行 |
| 旧 occupancyFencingToken 出现在任何盖章消息 | `rejected_stale_fencing_token` | Server 不接受状态变化，Client 不执行本地动作 |

网络分区期间 Client 本地不得接受旧 Occupancy token 的新 launch，Server 不在 Client
离线时把占用转交给其他用户；已运行 Worker 的执行结果经现有 ExecutionPort
outbox/replay 在恢复后上报，控制面只报告"状态暂不可确认"，不伪装 Running 或 Failed。

## 四类凭据分离

| Credential | 认证对象 | 使用位置 | 限制 |
| --- | --- | --- | --- |
| Browser Session Cookie | 用户（浏览器会话） | Public API、WebSocket | 永不出现在本端口与 ExecutionPort |
| Device Credential | ClientNode | 仅 `POST /internal/v1/client/exchange` | 只存设备本地；可经 `client.credential_rotate` 轮换 |
| Connect Code | 一次性建立用户访问关系 | 仅 Web 添加 Client 流程 | 一次性、短时、限次，Server 只保存摘要；不得认证 exchange 或 Worker |
| Worker Session Credential | 单个 WorkerSession | 仅 ExecutionPort | 短期有效，digest 绑定 WorkerLaunchGrant |

四类凭据不得相互复用。Client ID 公开、不保密，但永远不作为凭据；连接失败不得泄露
Client 是否属于某个用户。所有授权、撤销与轮换进入 Audit。
