# WinWinCode 架构总览

浏览器提交任务，Server 保存产品状态并派发执行，Device Client 在设备上启动 Worker，Worker 驱动内嵌 Codex 内核。

## 组件

WinWinCode 当前由浏览器 Client、Control Plane、Execution Worker 和设备 Client 组成。`apps/client` 是产品 Client。`winwincode-server` 是独立 Server。`winwincode-worker` 是独立 Worker。`winwincode-local` 提供把 Control Plane 和 Worker 组合到一个进程中的库接口。`wwc` 来自 `crates/winwincode-cli`。

身份模型保留单个本地 Owner。协议使用 organization、workspace、project、repository 四级作用域。`crates/winwincode-domain/src/generated.rs` 中的 `RepositoryScope` 携带四个标识。

```mermaid
flowchart LR
    Browser["Client\napps/client"]
    Server["Server\nwinwincode-server"]
    CP["Control Plane\nwinwincode-control-plane"]
    Local["Local launcher\nwinwincode-local"]
    Worker["Worker\nwinwincode-worker"]
    Device["Device Client\nwinwincode-device-client"]
    Provider["Provider runtime\nwinwincode-provider"]
    Kernel["Kernel wrapper\ncrates/kernel"]
    Helper["kernel-helper\ncrates/helper"]
    Codex["Embedded Codex\nthird_party/codex"]
    DB[("SQLite")]

    Browser -->|"HTTP commands and queries"| Server
    Server -->|"WebSocket events"| Browser
    Server --> CP
    CP --> DB
    Server <-->|"ClientControlPort HTTP exchange"| Device
    Device -->|"launch and supervise"| Worker
    Server <-->|"ExecutionPort HTTP exchange"| Worker
    Local --> CP
    Local -->|"in-process ExecutionPort"| Worker
    Worker --> Provider
    Worker --> Kernel
    Kernel --> Codex
    Kernel -->|"signed helper process"| Helper
    Device --> DB
    Worker --> DB
```

图中的 SQLite 节点代表多个本地数据库。它不是一个跨进程共享连接。

## Client 与 Server

`apps/client` 提供 Chat、任务、设置、审批、设备和 StrongFlow 页面。当前包已经依赖 React 18。主要装配入口是 `apps/client/src/application.ts`、`apps/client/src/boot.ts` 和 `apps/client/src/index.ts`。详细结构见 [前端架构](frontend.md)。

Client 通过 `packages/control-plane-client/src/index.ts` 和 `apps/client/src/community-control-plane-client.ts` 访问 Server。协议版本为 `winwincode/v1`。写操作进入 `/api/v1/commands`。读操作进入 `/api/v1/queries`。会话管理使用 `/api/v1/auth/session`。`/api/v1/events` 升级为 WebSocket，并推送持久事件。

`crates/winwincode-api/src/generated.rs` 定义公共 HTTP 和 WebSocket 类型。`crates/winwincode-server/src/server.rs::router` 绑定路由。`crates/winwincode-server/src/application.rs` 把传输请求交给 Control Plane 服务。

Server 对浏览器来源、会话和请求体大小做边界检查。远程 Worker 通道要求 TLS。远程预览在非回环地址上也要求 TLS。具体安全约束见 [安全模型](../security-model.md)。

## Control Plane 与 ExecutionPort

`crates/winwincode-control-plane` 持有产品编排和授权规则。它负责 Chat 会话、Delivery、StrongFlow 投影、Worker 注册、执行调度、Provider 准入、Publication 和审计协调。

ExecutionPort 是 Control Plane 与 Worker 之间的类型化协议。协议类型位于 `crates/winwincode-execution-port/src/generated.rs`。`crates/winwincode-control-plane/src/execution_port_service.rs::ExecutionPortService` 处理注册、心跳、租约、派发结果、运行时消息和动作授权。

组合库与远程执行使用同一组 `ExecutionPortMessage`：

- 同进程组合由 `crates/winwincode-local/src/lib.rs::LocalLauncher` 启动双方。`LocalExecutionPortHandle` 在进程内转发消息。
- 远程模式由 Server 的 `/internal/v1/execution-port/exchange` 提供有认证的 HTTP 交换。`crates/winwincode-server/src/remote_worker_transport.rs::ProductionRemoteWorkerExchange` 连接远程传输和 `RemoteWorkerPoolAdapter`。
- Worker 使用 `crates/winwincode-worker/src/lib.rs::WorkerMain` 管理注册、租约、工作区、取消、运行时事件和终态回执。

当前生产 Server 入口通过 `run_composed_server` 调用 `run_remote_composition`，提供远程执行和 Device 交换端点。它不读取 `WWC_SERVER_WORKER_MODE`。同进程组合库的存在不代表生产入口可以通过该变量切换模式。实际启动配置见 [部署指南](../operations/deploy.md)。

ExecutionPort 使用持久出站消息和确认游标。传输失败不会直接抹掉已接受的产品状态。Server 会恢复已经接受但尚未完成清理的结果。

## 设备路径与 Provider

设备 Client 使用 `/internal/v1/client/exchange` 与 Server 交换 `ClientControlPort` 帧。协议位于 `crates/winwincode-client-port`。Server 端实现位于 `crates/winwincode-server/src/client_exchange.rs::ClientExchangeApplication`。设备端守护逻辑位于 `crates/winwincode-device-client/src/daemon.rs`。

设备 Client 保存设备身份、连接游标、仓库路径映射、占用状态和 Worker 进程状态。它还能接收 Worker 启动、停止、Provider 配置、扩展配置和仓库注册命令。

Provider 的具体执行位置在 Worker。远程模式把 Worker 放在独立执行机器上。Device Client 可以监管设备上的 Worker。本地模式把 Worker 与 Control Plane 放在同一进程。`crates/winwincode-worker/src/device_model.rs::DeviceModels` 接收 `ModelOpenMessage`。它打开 `crates/winwincode-provider/src/device_store.rs::DeviceProviderStore`，再执行模型请求。`crates/winwincode-provider/src/provider_https_sse.rs::HttpsSseProviderAdapter` 负责 HTTPS/SSE Provider 调用和流转换。

Server 不直接持有设备 Provider 密钥。`crates/winwincode-server/src/device_providers.rs` 把加密配置写入设备下行队列。设备 Client 应用配置，并把快照和回执上报。模型输出以 `ModelChunkMessage` 经 ExecutionPort 返回 Worker 和 Control Plane。

## Kernel 与内嵌 Codex

`third_party/codex` 是内嵌 Codex 源码。其版本由 `upstream/sources.lock.json` 锁定。当前锁定版本是 Codex 0.149.0，提交为 `758ef40f50c1a458425c7cfbf1eb12cbc07af0b0`。

`crates/kernel` 是 WinWinCode 对内嵌 Codex 的所有权边界。它通过 `codex-core-api` 创建线程和回合，并把工具调用交给 WinWinCode 的动作授权。`crates/kernel/src/model_port.rs::KernelModelStreamTransport` 把 Codex 的 `ModelStreamTransport` 接到 WinWinCode `ModelPort`。

`crates/winwincode-codex` 把 Kernel 接入 Worker。它负责线程映射、执行回执、模型桥接、候选产物、运行时投影和持久 outbox。`crates/helper` 生成 `winwincode-kernel-helper`。该程序承载 Code Mode、沙箱和文件系统辅助入口。

## Chat、StrongFlow、Delivery 与 Publication

Chat 以 `ProductSession` 为产品会话。`crates/winwincode-session` 定义生命周期和执行会话绑定。`crates/winwincode-control-plane/src/product_session_service.rs` 保存消息、回合意图、取消路由和 Worker 绑定。Client 的入口位于 `apps/client/src/chat-page.ts` 和 `apps/client/src/chat-view-model.ts`。

Chat 提交后，Control Plane 创建执行作业。设备快速路径由 `crates/winwincode-control-plane/src/quick_device_execution.rs::dispatch_turn_to_device_worker` 绑定到设备 Worker。Worker 运行内嵌 Codex，并把助手输出和终态通过 ExecutionPort 返回。

Delivery 是可审计交付流程。领域模型位于 `crates/winwincode-delivery/src/domain`。Control Plane 负责命令事实、执行派发、审核结论和重做。Delivery 状态与 outbox 在同一产品事务中提交。

StrongFlow 通过 Delivery 编排交付流程。界面消费 Delivery 与运行时事实的读模型。`crates/winwincode-control-plane/src/strongflow_projection.rs::StrongFlowProjectionQueryPort` 读取 Delivery、WorkRun、候选差异、证据和运行时投影。Client 的审核页面位于 `apps/client/src/strongflow-review-detail.ts`。

Publication 是受策略约束的外部副作用。`crates/winwincode-publication` 定义 Publication、策略、操作和存储端口。`crates/winwincode-control-plane/src/publication_application.rs` 和 `publication_production.rs` 负责授权、执行和回执。Publication 只能基于已建立的 Delivery 与候选事实推进。

## SQLite 存储

Control Plane 使用 `crates/winwincode-storage/src/lib.rs::SqliteStorage`。默认数据库位于 Server 数据目录。打开数据库时启用外键、WAL 和 `synchronous=FULL`。该存储覆盖产品状态、事务回执、事件 outbox、执行队列、租约、Worker 注册、设备绑定、候选产物和 Publication。

设备 Client 使用 `crates/winwincode-device-client/src/store.rs::DeviceStore`。它使用独立 SQLite 数据库。它保存设备身份、上下行游标、仓库映射、占用镜像和 Worker 进程记录。

Provider 使用 `DeviceProviderStore` 保存设备侧 Provider 配置、调用回执和模型交换。`winwincode-codex` 也维护 Worker 侧执行状态。各数据库通过协议和回执协调，不共享内存事务。

审计数据由 `crates/winwincode-audit` 持久化。审计事件使用摘要链检测篡改。存储与恢复约束见 [系统不变量](../invariants.md) 和 [部署](../operations/deploy.md)。

## 网络策略

`crates/winwincode-network` 提供统一的网络失败分类、重试决策、可取消 I/O 和可选请求日志。`NetworkFailure` 记录失败种类、请求阶段、是否已被上游接受、HTTP 状态和 `Retry-After`。调用方据此决定立即重试、等待连接、重放或终止。

Provider 的 HTTPS/SSE 请求使用这一传输层。设备 Client 和 Worker 也复用失败分类。请求取消会关闭已登记的套接字，从而中止阻塞 I/O。

前端请求使用 `packages/network-request`。`scripts/generate-network-policy.mjs` 从 `schema/winwincode/network-request-policy.json` 生成 `packages/network-request/src/policy.generated.mjs`。Rust 网络层与前端请求层分别实现各自的传输接口。

执行中的网络动作还受 ExecutionPort 动作策略约束。`crates/winwincode-execution-port/src/action_normalizer.rs` 把网络、Shell、文件、Git 和 MCP 请求规范化。`WorkerActionGateway` 要求有效执行租约和动作授权回执。网络失败策略不能替代权限检查。

## 第一方 crate 依赖

下图只显示工作区内当前产品 crate 的 Cargo normal 依赖。箭头表示左侧 crate 依赖右侧 crate。可选 normal 依赖也包含在图中。第三方依赖和计划移除的 crate 不在图中。

```mermaid
flowchart LR
    helper["kernel-helper"]
    kernel["kernel"] --> execution["execution-port"]
    execution --> domain["domain"]
    api["api"] --> domain
    audit["audit"] --> domain
    delivery["delivery"] --> domain
    publication["publication"] --> domain
    clientport["client-port"] --> api
    network["network"]
    provider["provider"] --> api
    provider --> domain
    provider --> execution
    provider --> network
    storage["storage"] --> delivery
    storage --> domain
    storage --> execution
    storage --> publication
    device["device-client"] --> api
    device --> clientport
    device --> network
    device --> provider
    fusion["fusion"] --> delivery
    codex["codex adapter"] --> domain
    codex --> execution
    codex --> fusion
    codex --> kernel
    codex --> provider
    worker["worker"] --> codex
    worker --> domain
    worker --> execution
    worker --> network
    worker --> provider
    session["session"] --> domain
    repoctx["repository-context"]
    obs["observability-core"]
    evidence["evidence-export"]
    testassets["test-assets"] --> delivery
    testassets --> domain
    control["control-plane"] --> api
    control --> audit
    control --> clientport
    control --> delivery
    control --> domain
    control --> execution
    control --> fusion
    control --> network
    control --> provider
    control --> publication
    control --> repoctx
    control --> session
    control --> storage
    local["local"] --> control
    local --> obs
    local --> worker
    server["server"] --> api
    server --> clientport
    server --> codex
    server --> control
    server --> delivery
    server --> domain
    server --> execution
    server --> local
    server --> storage
    server --> worker
    cli["wwc CLI"] --> delivery
    cli --> device
    cli --> domain
    cli --> evidence
    cli --> repoctx
    cli --> server
    cli --> storage
    cli --> testassets
```

`kernel-helper` 只依赖内嵌 Codex crate 和 Tokio，因此图中没有第一方出边。

模块职责见 [crate 清单](crates.md)。架构决定和迁移范围见 [ADR](../adr/README.md)。
