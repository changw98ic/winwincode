# ADR-0028：TypeScript 表现层、Rust Control Plane 与 Rust Execution Worker

- 状态：已接受，正在迁移
- 日期：2026-08-24
- 对应 Epic：`winwincode-9c4.16`
- 企业协作后续：`winwincode-9c4.17`
- 当前阶段：`winwincode-9c4.16.1.1`
- 迁移清单：[`0028-control-plane-worker-migration.inventory.json`](0028-control-plane-worker-migration.inventory.json)
- 既有交付所有权：[ADR-0023](0023-canonical-delivery-ownership.md)

## 结论

WinWinCode 的统一目标架构是：

```text
TypeScript Presentation Layer
          ↓ HTTP / WebSocket
Rust Control Plane
          ↓ ExecutionPort
Rust Execution Worker
          ↓
Embedded Codex Core
```

Rust Control Plane 是全部产品和业务事实的唯一写入方。Rust Execution Worker
协调工作区与一次执行，但不拥有组织、交付、审批或长期凭据。Codex Core 继续是
Plan、Agent、工具、Shell、沙箱和代码执行事实的唯一权威。

本地版可以把 Control Plane 和 Worker 组合进同一个进程；企业版可以把两者部署为
独立进程和 Worker 池。两种部署使用同一份接口和状态语义，不维护一条本地专用后门。

## 背景

当前实现把 DSH 的 Session、Provider、Credential 和审批能力，与 WinWinCode 的
Delivery/StrongFlow TypeScript 服务、Node 原生桥接和 Rust Codex 内核串在一起。
这条路径已经可以运行，但关键产品状态分布在 TypeScript 与 Rust 两边，也把未来的
组织权限、远程 Worker、审计和集中模型治理绑定到一个本机 Node Host。

直接逐文件翻译只能改变语法，不能自动确定事实归属、持久化边界、取消语义或错误协议。
因此迁移采用“先冻结可观察行为，再翻译，再纠正行为差异”的顺序。现有 DSH 路径在
迁移期是行为样本来源，不是最终运行时。

## 组件职责

### TypeScript Presentation Layer

TypeScript 只负责页面、组件、路由、表单、图表、浏览器状态以及 HTTP/WebSocket
客户端。它只消费 API Projection，不持久化或判定完整领域对象，也不直接调用 Worker。

### Rust Control Plane

Control Plane 负责：

- HTTP Command、Query API 和 WebSocket Event Gateway；
- ProductSession、Delivery/StrongFlow、Approval、Attention 和 Publication；
- Identity、Organization、RBAC、Policy、Audit 与 Collaboration；
- Provider/Model Gateway、Credential 引用、预算与限流；
- Scheduler、Job、Lease、Fencing、Worker Registry 和取消协调；
- SQLite 本地存储与 PostgreSQL 企业存储的同一领域语义。

主要业务写入使用 HTTP Command，并携带 `requestId` 与 `expectedRevision`。WebSocket
发送状态投影、运行事件、审批请求、Attention、任务和在线状态，不作为主要业务写入
通道。

### Rust Execution Worker

Worker 负责：

- WorkerSession 生命周期、心跳、能力与容量上报；
- checkout、worktree、候选、Diff、运行产物和本机清理；
- 嵌入 Codex Core，执行工具、Shell、MCP 和沙箱操作；
- 按顺序上报运行事件，接收输入、审批结果、取消和模型流；
- 使用 Lease 与 Fencing Token 拒绝过期 Worker 的写入。

Worker 不保存 Organization、RBAC、Delivery 权威状态或长期 Provider 密钥。

### Codex Core

Codex Core 继续独占 Thread、Turn、Plan、Agent Graph、工具调用、Shell、MCP、沙箱、
文件与网络权限、Diff、用量和执行恢复事实。Control Plane 与 Worker 只保存必要引用、
运行包络和只读投影，不实现第二套 Agent 调度器。

## 唯一执行边界

Control Plane 与 Worker 只通过一个版本化 `ExecutionPort` 通信。该接口覆盖：

```text
register / capability / heartbeat
job / lease / fencing
runtime event / model stream
input / approval / cancel
outcome / artifact reference
```

Web 不直接连接 Worker。Worker 也不直接改写 Delivery 或 ProductSession。运行事件先由
Worker 带上 Job、Lease、WorkerSession 和 CodexThread 身份发送给 Control Plane，
再由 Control Plane 持久化并广播 Projection。

## 三种会话和一次阶段尝试

四个身份保持独立：

| 身份 | 所有者 | 含义 |
| --- | --- | --- |
| `ProductSession` | Control Plane | 用户看到的产品会话与消息入口 |
| `WorkerSession` | Execution Worker | 一次可租约、取消和恢复的 Worker 执行上下文 |
| `CodexThread` | Codex Core | Codex 的上下文、Turn 和执行历史 |
| `StageRun` | Delivery | 一个交付阶段的一次尝试 |

`SessionBinding` 绑定这些身份及当前 Delivery/Task，但不把它们压成一个通用
`session_id`。重启、重试、迁移 Worker 或重新验证时，各身份可以按自己的生命周期
变化。

## Provider 与 Credential

Worker 通过 ModelPort 请求模型，Control Plane 的 Provider Gateway 解析模型、预算、
限流和长期 Credential，再把流式结果返回 Worker。长期密钥只进入 Keychain、Vault
或 KMS 支持的 Credential 服务；Worker 不持有长期 Provider 密钥。

本地同进程部署仍经过相同端口调用，不通过共享全局变量绕开该边界。

## Canonical Schema

领域模型先投影成 API DTO，再从唯一 canonical schema 生成 Rust API 类型、OpenAPI、
JSON Schema、TypeScript 客户端类型和稳定错误码。前端只获得 Projection，不直接更新
完整 Domain Entity。

以下合同不得分别手写两份：

- ID、枚举和错误码；
- HTTP Command/Query DTO；
- WebSocket 事件；
- Delivery、StageRun、Approval 与 Runtime Event 的公开 Projection。

## 部署形态

### 本地部署

```text
winwincode 本地进程
├─ Control Plane 模块
└─ Embedded Local Worker 模块
   └─ Codex Core
```

本地部署可以共享进程和 SQLite，但模块间仍使用 `ExecutionPort`，并保留 Job、Lease、
Fencing 和身份字段。

### 企业部署

```text
Control Plane
├─ Worker A
├─ Worker B
└─ Worker Pool
```

企业部署可使用 PostgreSQL、对象存储、集中 Secret Store 和多个隔离 Worker。Worker
可以按平台、容量、网络区、仓库和策略分配，Control Plane 保持同一业务接口。

## 迁移顺序

迁移由 Beads Epic `winwincode-9c4.16` 管理，固定为六个阶段：

1. 冻结 canonical schema、`ExecutionPort`、可观察行为和错误语义；
2. 把 Delivery/StrongFlow 与本地工作区分别迁入 Control Plane 和 Worker；
3. 迁移 GitHub Publication、Audit 与 Policy；
4. 迁移 ProductSession、Approval、Scheduler 和 Worker 生命周期；
5. 迁移 Provider/Model/Credential，删除长期密钥进入 Worker 的可能路径；
6. 切换 TypeScript Web、移除 DSH Node/Cordis/N-API 后端并完成四平台发布门禁。

每个阶段先做结构翻译，再使用冻结样本纠正行为差异。阶段门禁同时检查新路径的行为、
旧路径调用方覆盖和临时适配器删除任务。

## 当前迁移基线与删除决定

机器可检查的清单
[`0028-control-plane-worker-migration.inventory.json`](0028-control-plane-worker-migration.inventory.json)
覆盖当前生产后端源码、DeepSeek 依赖、目标模块、迁移阶段和行为样本。基线固定成功、
失败、取消、恢复、审批与关闭六类外部结果。

以下内容只允许在迁移期存在，并在对应门禁删除：

- DSH/Cordis 后端组合与 remote 服务；
- Node 到 Rust 的 N-API 产品桥接与四个平台 Node 包装；
- TypeScript 中的 Provider、Credential、Session、Delivery 和 Publication 业务实现；
- 仅属于未使用 DSH 插件生态、没有 WinWinCode 产品调用方的能力。

需要跨进程、跨平台或外部系统存在的兼容，只能放在正式协议与 Adapter 边界；不得保留
两套产品合同或旧业务写入路径。

## 保持不变的决定

本决定改变后端语言和部署边界，不改变 [ADR-0023](0023-canonical-delivery-ownership.md)
定义的十对象 Delivery 模型，也不改变 Codex Core 的唯一执行权威。DeliveryTask 仍然是
产品级可独立验收单元；Codex Plan 仍然是一次执行内部计划。

## 影响

正面影响：

- 关键业务状态只有 Rust Control Plane 一个写入方；
- 本地版和企业版共享接口，不需要再次拆分执行内核；
- Worker 可独立扩缩容、隔离和恢复；
- Provider 密钥、预算、权限和审计集中管理；
- TypeScript 前端可由生成客户端约束，减少类型漂移。

迁移成本：

- DSH 已有的 Provider、Credential、Session 和插件行为需要重新实现并校准；
- 过渡期必须同时维护冻结基线和窄适配器；
- 同进程本地版也要承担明确 Job、Lease 和事件协议的实现成本；
- 删除旧后端前必须完成四平台构建、恢复、取消、审批和发布验证。
