# WinWinCode

WinWinCode 把"让 AI 写代码"变成一条有人工审核、有证据的交付流水线。

你用一段自然语言描述需求,系统先让 AI 生成实现方案,由你批准后,在隔离的工作区里完成编码和测试;再由独立的验证角色逐条核对验收条件,最后由你确认交付。每个交付结论都附带可核查的证据——Diff、测试与运行记录、审计摘要——而不是 AI 的一句"我做完了"。

它和"直接让 AI 改代码"的工具的区别在于:WinWinCode 不替你决定合入什么。它负责把需求变成方案、把方案变成候选、把候选变成带证据的交付,而每一个跨越都由人签字。

当前仓库正在准备 `0.1.0-alpha.1` 首个公开预览版,支持 Apple Silicon / Intel macOS 和 arm64 / x64 GNU Linux。

## 一个需求的旅程

1. 你在 Chat 页面提交需求,系统整理目标、范围和验收条件;
2. AI 生成实现方案和结构化图,你审核批准(不认可就要求修改);
3. AI 在隔离工作区执行,产出候选代码;
4. 独立验证角色逐条核对验收条件,形成证据和结论;
5. 你最终审核候选,批准后交付完成。

验证不通过不会悄悄带过:系统生成待处理的注意项或有限返工,历次尝试的记录保持不变。StrongFlow 页面完整展示每个交付的方案、图、验收条件、证据和结论。

## 系统架构

WinWinCode 由一个浏览器 Client 和一条 Rust 服务端路径组成:

```text
浏览器 Client(apps/client:Chat / StrongFlow / 设置 / 企业管理)
   │ HTTP + WebSocket
   ▼
Server(winwincode-server)—— 唯一公开入口:认证、健康检查、API
   └─ Control Plane —— 产品状态的唯一写入方:需求、方案、审批、调度、审计
        │ ExecutionPort
        ▼
      Worker(winwincode-worker)—— 执行事实的唯一上报方:隔离工作区、任务、租约、结果
        └─ Kernel —— 内嵌固定版本的 Codex Core:AI 在这里执行,是执行事实的唯一权威
```

三条所有权规则贯穿全系统:

- 所有产品状态只有 Control Plane 能写;
- 所有执行事实只有 Worker 能上报;
- 所有 Codex 执行事实只有 Kernel 是权威。

部署形态有两种:本机模式把 Control Plane 和 Worker 组装在同一进程(`winwincode-local`);企业模式把 Server 与多个 Worker 分进程、分机器部署。两种形态使用同一套合同与状态语义。

组件职责、交付数据模型、身份绑定、Provider 与凭据、安全模型和实现索引见[产品架构文档](docs/architecture.md)。

## 快速开始

这是仓库唯一的首次运行路径:安装依赖、构建 Client 和 Rust 运行组件,然后启动 `winwincode-server`。

### 1. 准备环境

需要:

- Node.js 24.x(发布构建固定使用 [`.node-version`](.node-version) 中的 24.19.0)、Corepack 和 pnpm 11.7.0;
- Rust 1.95.0,包含 `rustfmt`、`clippy` 和 `rust-src`;
- macOS 的 Xcode Command Line Tools,或 Linux 上的 C/C++ 编译器、`pkg-config` 和 libcap 开发文件。

Debian / Ubuntu 可以用以下系统包满足 Linux 构建要求:

```bash
sudo apt-get install build-essential pkg-config libcap-dev
```

### 2. 获取源码并构建

```bash
git clone https://github.com/changw98ic/winwincode.git
cd winwincode
corepack pnpm install --frozen-lockfile
corepack pnpm build
```

构建生成 `apps/client` 部署文件和 Rust 二进制。`winwincode-server` 启动前需要配置仓库根目录、租户范围、数据目录、认证信息、Helper 路径和模型路由,配置项见 [`crates/winwincode-server/README.md`](crates/winwincode-server/README.md)。

### 3. 启动并检查

```bash
corepack pnpm start
```

启动后浏览器加载 `apps/client` 静态文件,并通过运行时配置中的 `serverUrl` 连接 Server。以下纵向检查从 Server 入口走完"提需求到交付"的全流程,使用仓库内置的确定性测试输入,无需真实模型凭据:

```bash
corepack pnpm verify:api-production-vertical
```

真实模型执行需要配置可用的 Provider 和 Credential 引用。

## 当前状态与限制

- 本仓库处于 `0.1.0-alpha.1` 公开预览准备阶段,发布说明见 [docs/releases/0.1.0-alpha.1.md](docs/releases/0.1.0-alpha.1.md);
- Windows 暂不在首发平台;
- 本机模式面向单用户;企业部署复用同一套合同,把 PostgreSQL、对象存储、集中审计等作为可替换端口(见架构文档);
- GitHub 等外部写入需要人工批准,默认先生成本地 receipt 和审核证据。

## 参与贡献

贡献代码前阅读[参与指南](CONTRIBUTING.md),用 Beads 领取工作,并按 [Pull Request 模板](.github/pull_request_template.md) 记录实际检查结果。

常用检查:

```bash
corepack pnpm contracts:check
corepack pnpm format:check
corepack pnpm typecheck
corepack pnpm lint
corepack pnpm test
corepack pnpm build
corepack pnpm verify
```

## 文档

| 主题 | 文档 |
| --- | --- |
| 架构、交付流程与安全模型 | [docs/architecture.md](docs/architecture.md) |
| HTTP / WebSocket / ExecutionPort 合同 | [docs/contracts/](docs/contracts/) 与 [schema/winwincode/v1/](schema/winwincode/v1/) |
| 架构决定(ADR) | [docs/decisions/](docs/decisions/) |
| 发布门禁与发布流程 | [docs/release-gate.md](docs/release-gate.md)、[docs/releasing.md](docs/releasing.md) |
| 真实模型与真实仓库评估 | [docs/live-evaluation.md](docs/live-evaluation.md) |
| Server 配置 | [crates/winwincode-server/README.md](crates/winwincode-server/README.md) |
| 上游 Codex 版本、补丁与更新流程 | [upstream/sources.lock.json](upstream/sources.lock.json)、[docs/upstream-updates.md](docs/upstream-updates.md) |

## 仓库结构

```text
apps/client/   浏览器 Client(Chat、StrongFlow、设置、企业管理)
crates/        Rust 工作区:Server、Control Plane、Worker、Kernel 及支撑 crate
packages/      跨端共享包:contracts、strongflow
schema/        canonical schema 与生成产物,是所有合同类型的唯一来源
tests/         Client、Server、Worker 与合同检查
docs/          架构、合同、决定与发布文档
upstream/      固定上游身份、补丁和通知记录
```

crate 全量清单与依赖目标图见[机器可读源码清单](docs/decisions/0028-control-plane-worker-migration.inventory.json)和[目标图](docs/decisions/0028-control-plane-worker-target-graph.json)。

## 许可证

WinWinCode 自有代码使用 [Apache License 2.0](LICENSE)。Codex 和其他第三方组件的归属要求记录在 [NOTICE](NOTICE) 与 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 中,不构成 WinWinCode 项目的第二次许可声明。

公开协作与安全报告见[参与指南](CONTRIBUTING.md)、[安全报告](SECURITY.md)与[行为准则](CODE_OF_CONDUCT.md)。
