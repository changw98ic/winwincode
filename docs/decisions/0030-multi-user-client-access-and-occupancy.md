# ADR-0030：多用户共享 Client 的访问授权与独占占用

- 状态：已接受
- 日期：2026-09-05
- 对应任务：`winwincode-9fu.1`（CLIENT-000.1）
- 上层运行边界：[ADR-0028](0028-control-plane-worker-migration.md)
- Client 表现层边界：[ADR-0029](0029-client-ui-architecture.md)
- 执行合同：[ExecutionPort v1](../contracts/execution-port-v1.md)

## 背景

当前仓库只有单一用户假设：Browser Client 通过 Server 访问本机 Control Plane 与 Worker，
"设备在线、用户已连接、正在占用、正在执行"混在同一连接语义里。多用户共享 Client 的目标
是：公网 Server 上的任意已登录用户可以连接一台本地 Device Client；同一 Client 可授权多个
用户，但同一时刻只有一个用户占用；占用者可在 Client 容量内并发运行多个 WorkerSession；
Codex 始终修改 Client 所在机器上的本地 Git 仓库。

这个目标要求把"在线、连接、占用、执行"拆成四个有独立所有者和生命周期的状态，而不是继续
糊成一个 `connected=true`。本决定只冻结这四层模型、权威所有权表、占用协议与 fencing 语义；
schema、合同生成和状态机冻结由 Phase 0 其余任务完成。既有权威边界不被重写。

## 决定

### 1. 四层连接模型

```text
ClientNodePresence
设备是否在线
        ↓
ClientAccessGrant
某用户是否被允许使用
        ↓
ClientOccupancyLease
当前由哪个用户独占
        ↓
WorkerSession
实际运行哪些任务
```

| 层 | 含义 | 关键状态/字段 | 分离理由 |
| --- | --- | --- | --- |
| `ClientNodePresence` | 机器级常驻状态 | `pending_enrollment / online / degraded / offline / locked / revoked` | Device Client 可以在无人操作时常驻在线；它不属于任何 Browser Session。"设备在线"不等于"某用户已连接"，浏览器全部关闭也不改变 presence |
| `ClientAccessGrant` | 用户与 Client 的多对多授权关系 | `permissions: [use, manage, share]`、`trustMode: temporary/trusted`、`state: active/revoked/expired` | 一个 Client 可被多个用户授权使用；授权是持久关系，授予与撤销都不产生也不终止占用 |
| `ClientOccupancyLease` | 当前独占使用权 | `available / reserving / occupied / draining / recovery_pending / released / expired`、`holderUserId`、`fencingToken` | 一个 Client 同一时刻最多一个活动 Lease；占用属于 `userId`，不属于某个浏览器 cookie。占用者的多个浏览器/设备控制同一个占用，不按标签页重复占用 |
| `WorkerSession` | 实际执行单元 | 一个 `WorkerSessionId` 对应一个 Worker 进程和一个 CodexThread 生命周期 | 占用独占和 Worker 并发不是一回事。占用者可在容量内并发运行多个 WorkerSession，Worker 不跨 WorkerSession 混用上下文 |

四层各自可独立失败：Client 掉线是 presence 事实，不收回 AccessGrant，也不立即销毁
Lease（进入 `recovery_pending`）；浏览器关闭不影响占用与正在运行的任务；授权撤销立即生效，
但本地清理仍由 Device Client 执行。`ProductSession`、`WorkerSession`、`CodexThread`、
`StageRun` 继续保持独立身份（沿用 ADR-0028 的四身份表）。

### 2. 权威所有权

| 事实 | 权威所有者 |
|---|---|
| 用户、Client 授权、Client 占用 | Control Plane |
| Client 在线、平台、版本、容量 | Device Client 上报，Control Plane 持久投影 |
| 本地仓库绝对路径 | Device Client 本地数据库 |
| RepositoryBinding 产品身份与安全元数据 | Control Plane |
| Worker 进程和本地 PID | Device Client |
| WorkerSession、Lease、Fencing、Job | 现有 Worker / Control Plane 合同 |
| Codex Thread、Turn、Plan、工具状态 | Kernel |
| Candidate 本地 Git ref | Device Client / Worker |
| Candidate、Evidence、Verdict 产品结论 | Control Plane |

这张表是权威分配，不是实现建议。任何模块引入上表之外的所有权声明，或让一个事实出现两个
写入方，都必须先修改本 ADR。

### 3. 占用 = Control Plane 权威 + Device Client 执行

占用状态不是 Server 单方面声明，也不是 Client 本地自治，而是两阶段的权威/执行分工：

1. Control Plane 原子创建占用 Lease：校验 Browser Session、`use` 权限的
   `ClientAccessGrant`、Client 在线且未锁定、无活动占用 Lease、有可用 Session 槽位，
   然后创建 `reserving` Lease 并生成新 fencing token；
2. Device Client 通过 ClientControlPort 收到 occupancy offer，持久保存占用镜像并
   ACK 对应 fencing token；
3. ACK 后 Lease 才进入 `occupied`。未 ACK 的 Lease 不进入 `occupied`，Web 也不允许
   用户查看获授权仓库或创建任务；
4. Device Client 拒绝任何不匹配当前占用 Lease 的 Worker Launch；
5. stale Lease 即使在 Server 上被重放，也不能在 Client 落地。

配套规则：

- 释放时无活动任务走 `occupied → released`；有活动任务走 `occupied → draining`：
  不接受新 WorkerSession，当前任务继续，全部终态后自动释放；
- Client 掉线进入 `recovery_pending`：不允许其他用户抢占、不允许新任务；Client 在恢复
  窗口内以同一 identity/generation 重连后按本地 Worker 事实对账（`still_running /
  terminal / missing / unknown`），对账成功恢复 `occupied` 或进入 `draining`；超过恢复
  期限也不自动把未知本地任务交给新用户，必须管理员/原占用者执行安全清理；
- 非占用者对被占用 Client 只看到"正在使用"，不暴露占用者的任务、Diff 和 Evidence。

### 4. Fencing token 单调递增与 stale 拒绝

- 每次新占用生成比之前更高的 fencing token；token 单调递增是全局规则，不是按会话局部
  计数；
- Device Client 对以下四类命令强制校验当前本地占用镜像中的 fencing token：Worker
  launch、Worker stop、Candidate apply、Repository mutation；
- 旧 token 永远拒绝，避免网络重放让前任占用者复活；网络分区期间 Client 本地也不得接受
  旧 Occupancy token 的新 launch；
- 占用镜像（occupancy mirror）持久保存在 Device Client 本地 SQLite；Client 重启后先
  扫描本地状态并向 Server 提交 `client.worker.reconcile`，未完成对账前
  `presence = degraded`、`occupancy = recovery_pending`，本地继续拒绝旧 token 命令；
- `WorkerLaunchGrant` 绑定 `clientNodeId`、`clientInstanceId`、`occupancyLeaseId`、
  `occupancyFencingToken`、`repositoryBindingId` 及全部执行身份，任何字段不一致均拒绝。
  Worker/Job 层的既有 Lease/Fencing 合同（ADR-0028）继续有效，占用 fencing 是在其上
  新增的一层，不替代它。

### 5. 绝对路径只留在 Device Client

- 本地仓库绝对路径只保存在 Device Client 本地 SQLite 的 path mapping 中；
- Server 不保存、不返回本地绝对路径：`RepositoryBinding` 在 Server 侧只有产品身份与
  安全元数据（commit、branch、dirty projection、binding identity），没有 `absolutePath`；
- Worker 的 managed-session 配置文件（权限 `0600`）中 `sourceDirectory` 只由 Device
  Client 写入，Server 不直接提供绝对路径；
- Web UI 与公开投影不显示绝对路径；
- Phase 0 增加 source-boundary lint，防止绝对路径进入公开合同与投影；Server DB、日志、
  浏览器 DOM/storage、HTTP/WebSocket frame 和 Audit export 的隐私扫描均不得出现绝对
  本地路径与各类明文凭据。

### 6. 保留边界

本决定不重写既有权威边界：

- Codex Core（`winwincode-codex`/`winwincode-kernel`）继续独占 Codex Thread、Turn、
  Plan、工具、Shell、沙箱、Diff、用量与执行恢复；
- ExecutionPort v1 继续是 Worker 与 Control Plane 之间唯一的执行消息合同；V1 中每个子
  Worker 继续直接使用现有 Remote ExecutionPort exchange 连接 Server，Device Client
  只负责启停、身份、repo 路径、占用校验、本地状态和 Candidate 应用，不代理模型流与
  Runtime frame；
- Evidence/Verdict 的产品结论权威仍在 Control Plane；Candidate 本地 Git ref 由
  Device Client/Worker 保留，但 Candidate、Evidence、Verdict 的产品结论归属不变；
- `winwincode-local` 保持 Control Plane + Worker 的单机同进程组合，不改造成 Device
  Client；多用户能力通过新增 `winwincode-client-port` 与 `winwincode-device-client`
  引入；
- 保留 Remote Worker 路径：新增 Worker Session Credential 与 LaunchGrant 验证，
  不重写传输。

### 7. V1 不做的事

V1 主链不包含：

- Electron 桌面主界面；
- 远程桌面、视频串流、P2P 打洞、键鼠控制；
- 浏览器直接读取任意本地目录；
- 云端源码托管或云开发容器；
- 公共注册、邮箱找回、OAuth、SSO、SCIM；
- Billing；
- Fusion、Adaptive Review、模型委员会；
- Windows；
- 直接覆盖用户当前 Dirty Worktree；
- 自动把全部本地仓库暴露给所有获准使用 Client 的用户。

借鉴 UU 远程的只是"设备 ID、动态验证码、设备列表、可信授权、占用与锁定"的连接体验，
不复制远程桌面传输栈。

## 后果与取舍

收益：

- 在线、授权、占用、执行各有唯一所有者和独立状态机，消除 `connected=true` 式的混合状态，
  多浏览器恢复控制、浏览器关闭不杀任务、掉线不抢占等不变量有了明确的结构支撑；
- "Control Plane 权威 + Device Client 执行 + ACK 后 occupied"让占用同时具备原子仲裁
  （单活动 Lease）与本地强制（fencing 拒绝），Server 重放与网络分区都无法让旧占用者在
  本地复活；
- 绝对路径与凭据留在 Device Client，Server 与公开投影天然满足隐私扫描要求，公网部署
  不泄露本地文件系统结构。

代价与风险：

- 占用变成两阶段协议（`reserving` → ACK → `occupied`），claim 延迟增加，并要求新的
  ClientControlPort 承载 offer/ACK/replay 与心跳，Device Client 侧需要本地 SQLite、
  持久镜像和重启对账，成为有状态组件，带来备份、修复与版本兼容负担；
- fencing 校验分散在 Device Client 的四类命令路径上，与 Control Plane 的授权检查形成
  双重校验，逻辑存在重复，需要合同测试保证两侧对"当前 Lease"的判定一致；
- `recovery_pending` 期间禁止抢占意味着新用户必须等待恢复窗口或管理员清理，牺牲可用性
  换取"不把未知本地任务交给新用户"的安全不变量；
- 容量不超卖要求 Control Plane 的 durable reservation 与 Device Client 的本地资源校验
  保持一致，两处容量账本需要在对账与测试中持续验证。

2026-09-05 Phase 0 Gate 通过后状态改为"已接受"：schema 冻结（client-control.schema.json）、
Rust/TS 合同生成与 round-trip、状态机与协议合同、ExecutionPort 边界守卫、source-boundary
lint 全部合入 main；Node 727/727、合同测试 36/36、winwincode-client-port 27/27 全绿。
