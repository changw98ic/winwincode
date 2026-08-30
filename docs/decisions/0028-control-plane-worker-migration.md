# ADR-0028：apps/client 与 Rust Server、Worker、Local、Helper 单一路径

- 状态：已接受，单一路径已冻结
- 日期：2026-08-30
- 对应阶段：`winwincode-9c4.16.6.6`
- 企业协作后续：`winwincode-9c4.17`
- 机器可读源码清单：[`0028-control-plane-worker-migration.inventory.json`](0028-control-plane-worker-migration.inventory.json)
- 机器可读目标图：[`0028-control-plane-worker-target-graph.json`](0028-control-plane-worker-target-graph.json)
- 既有交付所有权：[`ADR-0023`](0023-canonical-delivery-ownership.md)

## 决定

WinWinCode 只发布一条产品路径：

```text
apps/client
    │ generated HTTP / WebSocket client
    ▼
winwincode-server
    │
    ├─ winwincode-control-plane
    │      │ ExecutionPort
    │      ▼
    └─ winwincode-worker
           ├─ winwincode-codex → winwincode-kernel
           └─ winwincode-kernel-helper

winwincode-local 负责本机组装 Control Plane 与 Worker。
```

`apps/client` 是唯一 TypeScript 表现层；`packages/contracts` 和 `packages/strongflow` 只提供共享类型与 Delivery domain/projection 合同，不持有网络或运行时权限。`winwincode-server` 是唯一公开网络边界；
`winwincode-control-plane` 是产品状态、策略、凭据引用、调度、交付和审计的唯一写入方；
`winwincode-worker` 是工作区和一次执行的唯一协调方；`winwincode-kernel` 是 Codex 执行事实的唯一权威；`winwincode-kernel-helper` 只提供经过身份校验的辅助可执行文件。

本地部署把 Server 所需的 Control Plane 和 Worker 组合在一个 Rust 进程中；企业部署可以把同一模块放入多个进程。两种形态共用 schema、HTTP、WebSocket、ExecutionPort、收据和状态语义。

## 所有权边界

### `apps/client`

Client 负责页面、路由、表单、图表、浏览器状态和访问 `serverUrl`。它只读取 API Projection，不能保存完整领域状态、作业务判定或直接访问 Worker。`apps/client/src/generated` 是 HTTP、WebSocket 类型与传输实现的唯一生成目录；页面模块只依赖 [`control-plane-client.ts`](../../apps/client/src/control-plane-client.ts) facade。

### `winwincode-server`

Server 负责 TLS/HTTP/健康检查、认证会话、请求相关性、范围校验和 WebSocket 事件出口。请求进入 [`GeneratedContractDispatcher`](../../crates/winwincode-server/src/dispatcher.rs) 后只调用 Control Plane 的应用端口；Server 不创建第二套业务模型或执行协议。

### `winwincode-control-plane`

Control Plane 负责：

- `ProductSession`、`Delivery`、`StageRun`、`AttentionItem`、`EvidenceRef`、`CriterionResult` 与 `DeliveryVerdict`；
- HTTP Command、Query、WebSocket Projection、事件收据和请求幂等；
- Provider/Model 路由、Credential 引用、预算、限流、Approval、Policy、Audit 和 Publication；
- Scheduler、Job、Lease、Fencing、Worker Registry、取消和重启恢复；
- 绑定到明确 Git commit 的只读 Repository Context；
- SQLite 本地存储与企业存储的同一状态语义。

Control Plane 不拥有 Codex Thread、Turn、工具、Shell、沙箱或代码执行事实。它接收 Worker 的带身份结果，验证当前 Job、Lease 和 Fencing 后再写入产品状态。

### `winwincode-worker`

Worker 负责 WorkerSession、Job、Lease、Fencing、checkout/worktree、候选、Diff、产物、运行事件、模型流、取消和清理。它直接组合 `winwincode-codex`，但不持有产品 Delivery 状态、组织策略或长期 Provider 密钥。所有跨边界消息遵循 [`ExecutionPort v1`](../contracts/execution-port-v1.md)。

### `winwincode-kernel` 与 Helper

`winwincode-kernel` 直接拥有 CodexThread、Turn、Plan、Agent Graph、工具、Shell、MCP、沙箱、权限、Diff、用量和执行恢复。`winwincode-codex` 只翻译 ExecutionPort 与 Kernel 的 typed frame，并校验 `winwincode-kernel-helper` 的签名、版本、来源摘要、大小和握手身份。

Helper 不读取产品状态，不监听网络，也不处理业务请求。Helper 的二进制身份和发布清单在执行前验证；Server 和 Worker 只传递已验证的 Helper 路径。

### `winwincode-local`

Local 只做进程配置、数据目录、生命周期和模块组合。它的产品依赖固定为 `winwincode-control-plane`、`winwincode-worker` 与 `winwincode-observability`；业务状态、Provider 路由、工作区操作和 Kernel 适配留在对应所有者中。

## 会话与一次尝试

`ProductSession`、`WorkerSession`、`CodexThread` 和 `StageRun` 是四种独立身份。`SessionBinding` 另外记录 Delivery/Task、ExecutionJob、Lease、Fencing 和当前 WorkerInstance：

| 身份 | 所有者 | 生命周期 |
| --- | --- | --- |
| `ProductSession` | Control Plane | 产品会话和消息入口 |
| `WorkerSession` | Worker | 当前 Worker 执行上下文 |
| `CodexThread` | Kernel | Codex 上下文、Turn 和执行历史 |
| `StageRun` | Delivery | 一次阶段尝试 |

新 retry 使用 attempt+1，建立新的 Job、Lease、WorkerSession、CodexThread 与 `SessionBinding`。旧 attempt 的 runtime、outcome 和 cancel 记录在终态后不再写入。重复请求由原始 receipt 响应，重启只重放未发布 outbox 事件，不重新执行已结算操作。

## 公开合同

所有公开 ID、枚举、错误、Command、Query、WebSocket payload 和 ExecutionPort frame 从 [`schema/winwincode/v1`](../../schema/winwincode/v1/README.md) 生成：

- TypeScript：[`apps/client/src/generated`](../../apps/client/src/generated/contracts.ts)；
- Rust transport：[`winwincode-api`](../../crates/winwincode-api/src/generated.rs)；
- Rust domain：[`winwincode-domain`](../../crates/winwincode-domain/src/generated.rs)；
- OpenAPI、JSON Schema 和 schema collection：[`schema/winwincode/v1`](../../schema/winwincode/v1/openapi.generated.json)。

HTTP Command 携带 `requestId` 和 `expectedRevision`。WebSocket 只发送已持久化的投影、运行事件、审批、Attention、任务和在线状态。Worker 的结果必须带 Job、Lease、Fencing、WorkerSession 和 CodexThread 身份；过期或跨范围消息在提交前失败。

## 依赖规则

[目标模块图与依赖门禁](0028-control-plane-worker-dependency-rules.md)和目标图共同冻结以下方向：

1. Control Plane 可依赖共享类型、存储、Delivery、Session、Publication、Audit 和 Repository Context，但不能到达 Kernel 或 Codex 执行模块。
2. Worker 只依赖 `winwincode-codex`、`winwincode-domain` 和 `winwincode-execution-port`；业务持久化与策略留在 Control Plane。
3. `apps/client` 只能经生成 Client 使用 `control-plane-http` 与 `control-plane-websocket`。
4. Local 只组装 Control Plane、Worker 和 Observability。
5. Server 可组合公开边界所需的 Control Plane、Worker、Codex、Local、Storage 和公共类型；它不另写产品状态。
6. Helper 只作为 Kernel 执行边界的已认证可执行文件。

`allowedInternalDependencies` 是精确清单，不是建议清单。新增 Rust 产品引用必须先更新目标图、源码清单和门禁测试。目标图同时记录 workspace 中现存的 enterprise/support crate；这些节点不会扩大 Server、Control Plane、Worker、Local 或 Helper 的核心允许边。

## 部署与安全

本机路径为：

```text
winwincode-local
├─ winwincode-control-plane
└─ winwincode-worker
   ├─ winwincode-codex
   ├─ winwincode-kernel
   └─ winwincode-kernel-helper
```

Server 公开一个配置的 HTTPS origin。浏览器会话只使用生成 Client 派生的 `serverUrl`；Credential 只以受保护引用存在于 Control Plane 的 Provider Gateway。审计记录保存主体、范围、操作、结果和摘要，不保存 prompt、模型正文、命令正文或 secret。

人工审核绑定当前 ProductSession、StageRun、Delivery revision、候选提交和审查集合摘要。`requirements`、`solution`、`planner` 使用只读工作区，`executor` 与 `remediator` 使用候选写入工作区，`reviewer`、`verifier` 与 `adversarial-verifier` 使用候选只读工作区。

## 验收与发布

源码、目录、依赖和文档的事实由 [migration inventory](0028-control-plane-worker-migration.inventory.json)、[target graph](0028-control-plane-worker-target-graph.json)、[`tests/control-plane-worker-dependency-contract.test.mjs`](../../tests/control-plane-worker-dependency-contract.test.mjs) 和 [`tests/control-plane-worker-inventory.test.mjs`](../../tests/control-plane-worker-inventory.test.mjs) 一起检查。Client、Server、Worker、Local 和 Helper 的组合门禁还检查：

- generated contracts 与 OpenAPI freshness；
- Server 的 HTTP、WebSocket、认证和错误分类；
- 每个 attempt 的 SessionBinding、dispatch、terminal outcome、cancel 和 restart exact；
- outbox/response-loss/duplicate 的 receipt-first 语义；
- foreign/tamper frame 拒绝、旧 attempt 零写和资源归零；
- `cargo metadata --locked`、Rust format、TypeScript source checks 和文档链接。

完成标记来自 Beads 与发布门禁；目录名本身不代表功能已经通过。
