# WinWinCode

WinWinCode 把自然语言需求保存为可审核的 Delivery，经过方案审核、执行、独立验证和交付审核后，形成带直接证据的交付结论。产品由一个浏览器 Client 和一条 Rust Server、Control Plane、Worker、Local、Kernel Helper 路径组成。

当前仓库正在准备 `0.1.0-alpha.1` 首个公开预览版，支持 Apple Silicon / Intel macOS 和 arm64 / x64 GNU Linux。

## 产品边界

| 组件 | 责任 |
| --- | --- |
| `apps/client` | Chat、StrongFlow、设置、审批和企业管理页面；通过一个 `serverUrl` 使用生成的 HTTP/WebSocket Client |
| `winwincode-server` | 唯一公开 HTTP、WebSocket、健康检查和认证边界 |
| `winwincode-control-plane` | ProductSession、Delivery、策略、Provider、Credential 引用、Scheduler、Publication、Audit 和全部产品状态 |
| `winwincode-worker` | WorkerSession、Job、Lease、Fencing、工作区、候选、运行事件、结果和清理 |
| `winwincode-kernel` | Codex Thread、Turn、Plan、Agent Graph、工具、Shell、沙箱、权限、Diff、用量和恢复事实 |
| `winwincode-kernel-helper` | 经过身份校验的执行辅助程序 |
| `winwincode-local` | 本机进程配置以及 Control Plane 与 Worker 的组合、启动和停止 |

Control Plane 是产品状态的唯一写入方，Worker 是执行事实的唯一上报方，Kernel 是 Codex 执行事实的唯一权威。完整所有权和安全边界见[产品架构、交付流程与安全模型](docs/architecture.md)。

## 用户会看到什么

- **Chat**：在 `apps/client` 的 `/chat` 页面提交需求、查看会话和运行状态。
- **StrongFlow**：在 `/strongflow` 页面查看 Delivery、方案、图、验收条件、Evidence 和 Verdict。
- **设置与审批**：在 `/settings` 和 `/approvals` 管理模型路由、权限决定和待处理 Attention。
- **企业管理**：在 `/enterprise` 查看组织、项目、Worker、策略、审计和外部连接状态。

所有页面都使用一个生成式请求 facade；页面不直接触碰 Worker、ExecutionPort 或 Rust 内部状态。

## 快速开始

这是仓库唯一的首次运行路径：安装依赖、构建 Client 和 Rust 运行组件，然后启动 `winwincode-server`。

### 1. 准备环境

需要：

- Node.js 24.x；发布构建固定使用 [`.node-version`](.node-version) 中的 24.19.0；
- Corepack 和 pnpm 11.7.0；
- Rust 1.95.0，包含 `rustfmt`、`clippy` 和 `rust-src`；
- macOS 的 Xcode Command Line Tools，或 Linux 上的 C/C++ 编译器、`pkg-config` 和 libcap 开发文件。

Debian / Ubuntu 可以用以下系统包满足 Linux 构建要求：

```bash
sudo apt-get install build-essential pkg-config libcap-dev
```

确认工具版本：

```bash
node --version
corepack pnpm --version
rustc --version
```

### 2. 获取源码并构建

```bash
git clone https://github.com/changw98ic/winwincode.git
cd winwincode
corepack pnpm install --frozen-lockfile
corepack pnpm build
```

构建会生成 `apps/client` 的部署文件，以及 Rust Server、Worker 和 Worker 内部 Kernel Helper 二进制。Local 模式以 `winwincode-server` 为启动入口，由 Server 通过 `winwincode-local` library 在同一进程内组装。Server 启动前必须配置仓库根目录、租户范围、数据目录、认证信息、Helper 路径以及模型路由；配置名和安全边界见 [`crates/winwincode-server/README.md`](crates/winwincode-server/README.md)。

### 3. 启动产品

```bash
corepack pnpm start
```

启动后，浏览器加载 `apps/client` 的静态文件，并使用运行时配置中的 `serverUrl` 连接 Server。HTTP Command、Query 和 WebSocket 事件都经过同一个生成 Client。需要关闭服务时使用 Server 的正常终止流程；Local 组合模式仍沿用相同的 typed frame、Lease、Fencing 和收据语义。

### 4. 运行生产纵向检查

```bash
corepack pnpm verify:api-production-vertical
```

该检查从 Server 入口验证 Client 可用的 Chat 与 StrongFlow API，覆盖认证、SessionBinding、Job dispatch、终态结果、取消、重启、receipt-first outbox、重复请求和资源释放。执行不需要真实模型凭据时，使用仓库提供的确定性测试输入。

## 交付生命周期

1. 确认 `DeliverySpec`、范围、仓库基线和验收条件；
2. Planner 生成方案和结构化图，人工审核当前方案集合；
3. Control Plane 创建当前阶段的 Job、Lease、Fencing 和 SessionBinding；
4. Worker 在隔离工作区执行，Kernel 产生运行事件和候选结果；
5. Reviewer 与 Verifier 在候选只读上下文中形成 Evidence 和逐项 Verdict；
6. 失败时创建 attempt+1，旧 attempt 的运行、结果和取消事实保持不变；
7. 所有必需条件通过后，人工审核当前候选并进入 `delivered`。

重启只恢复持久 receipt、outbox、SessionBinding、Job 和工作区事实，不重复已结算操作。重复 `requestId` 返回原始结果，过期 Lease 或跨范围消息在提交前失败。

## 当前能力

- 固定版本的 Codex Core 直接嵌入 Rust Kernel，不依赖外部编程 Agent 或命令行进程；
- 统一的 HTTP、WebSocket、ExecutionPort 和 canonical schema；
- ProductSession、WorkerSession、CodexThread、StageRun 的独立身份绑定；
- Delivery 方案审核、人工 Attention、有限返工、独立 Reviewer/Verifier 和最终 Verdict；
- 候选、Diff、运行事件、Evidence、发布 receipt 和审计摘要的可追溯链路；
- 本机单进程 Local 组装和企业 Server/Worker 分进程部署；
- 四个平台 Rust 构建目标与可复现的发布证据。

## 当前限制

- Windows 尚未进入首发平台；
- 当前本机配置面向单用户，Organization、共享数据库、RBAC、SSO、多租户隔离和跨机器调度属于企业部署范围；
- 真实模型执行需要配置可用的 Provider 和 Credential 引用；无密钥检查只验证流程和边界；
- GitHub 等外部写入需要当前人工批准，默认先生成本地 receipt 和审核证据；
- Jira、Linear、Slack 和 Teams 仍是外部协作系统，连接器需按企业合同单独配置。

## 开发与验证

贡献代码前阅读[参与指南](CONTRIBUTING.md)，用 Beads 领取工作，并按 [Pull Request 模板](.github/pull_request_template.md) 记录实际检查结果。

常用检查：

```bash
corepack pnpm contracts:check
corepack pnpm format:check
corepack pnpm typecheck
corepack pnpm lint
corepack pnpm test
corepack pnpm build
corepack pnpm verify
```

源代码清单、目标图和依赖合同见：

- [ADR-0028 单一路径决定](docs/decisions/0028-control-plane-worker-migration.md)；
- [机器可读源码清单](docs/decisions/0028-control-plane-worker-migration.inventory.json)；
- [机器可读目标图](docs/decisions/0028-control-plane-worker-target-graph.json)；
- [模块依赖门禁](docs/decisions/0028-control-plane-worker-dependency-rules.md)；
- [Control Plane HTTP 合同](schema/winwincode/v1/control-plane-http.schema.json)；
- [Control Plane WebSocket 合同](docs/contracts/control-plane-websocket.md)；
- [ExecutionPort 合同](docs/contracts/execution-port-v1.md)；
- [产品发布门禁](docs/release-gate.md)；
- [发布流程](docs/releasing.md)；
- [真实模型与真实仓库评估](docs/live-evaluation.md)；
- [确定性 Delivery fixture](docs/decisions/0024-deterministic-delivery-fixture.md)。

## 仓库结构

```text
apps/client/                         TypeScript 浏览器 Client 与生成请求 facade
crates/winwincode-server/            Rust HTTP、WebSocket、认证与健康边界
crates/winwincode-control-plane/     Rust 产品状态、策略、调度与外部治理
crates/winwincode-worker/            Rust Job、Lease、工作区、结果与运行事件
crates/winwincode-local/             Rust 本机 Control Plane/Worker 组合
crates/winwincode-codex/             Rust ExecutionPort 到 Kernel 的适配器
crates/kernel/                       Rust Codex Core 执行事实边界
crates/helper/                       Rust Kernel Helper 可执行程序
crates/winwincode-domain/             canonical schema 生成的共享 ID 与值对象
crates/winwincode-api/                canonical HTTP/WebSocket Rust 类型
crates/winwincode-execution-port/     Worker 控制与运行 frame
crates/winwincode-delivery/           Delivery 状态和阶段规则
crates/winwincode-session/            ProductSession 与身份绑定
crates/winwincode-storage/            state、receipt、journal 和 outbox 持久化
crates/winwincode-publication/        外部发布 receipt 与效果协调
crates/winwincode-audit/              审计链、retention 与导出
crates/winwincode-repository-context/ Git commit 绑定的只读仓库事实
schema/winwincode/v1/                唯一 canonical schema 和生成产物
upstream/                             固定上游身份、补丁和通知记录
tests/                                Client、Server、Worker 和合同检查
```

## 设计与许可证

详细设计见[产品架构](docs/architecture.md)、[ADR-0023 Delivery 所有权](docs/decisions/0023-canonical-delivery-ownership.md)和 [ADR-0027 发布门禁](docs/decisions/0027-product-release-gate.md)。上游 Codex 的版本、补丁和许可证义务见 [upstream 记录](upstream/sources.lock.json)。

WinWinCode 自有代码使用 [Apache License 2.0](LICENSE)。Codex 和其他第三方组件的归属要求记录在 [NOTICE](NOTICE) 与 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 中。

公开协作与发布信息见[参与指南](CONTRIBUTING.md)、[安全报告](SECURITY.md)、[行为准则](CODE_OF_CONDUCT.md)、[发布说明](docs/releases/0.1.0-alpha.1.md)、[发布流程](docs/releasing.md)和[上游更新流程](docs/upstream-updates.md)。
