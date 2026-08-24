# WinWinCode

WinWinCode 是建立在 Codex Core 执行内核和 DeepSeek Harness 产品外壳之上的交付控制层。它把自然语言需求保存为可审核的交付目标，用跨 Session 阶段控制方案、执行和返工，再用直接证据逐项形成交付结论。

当前仓库正在准备 `0.1.0-alpha.1` 首个公开预览版，支持 Apple Silicon / Intel macOS 和 arm64 / x64 GNU Linux。

## 产品边界

| 层 | 负责什么 |
| --- | --- |
| Codex Core | Plan、Agent Graph、多 Agent、工具、Shell、MCP、沙箱、权限、Diff、用量和执行恢复 |
| DeepSeek Harness（DSH） | 默认 Chat、Session、模型与 Provider 设置、凭据、执行审批交互和 Web/Cordis 外壳 |
| WinWinCode | `DeliverySpec`、验收条件、跨 Session 阶段、业务 Attention、Evidence 和 `DeliveryVerdict` |

Codex Core 是唯一执行权威。StrongFlow 只把 Codex Plan、Agent Graph 和运行事件投影成交付视图；DSH 继续管理模型、凭据、普通会话和审批界面。

详细的三层所有权、十个业务对象、状态流转、人工责任、执行图和安全边界见 [产品架构、交付流程与安全模型](docs/architecture.md)。

## 用户会看到什么

- **默认入口：DSH Chat。** 启动后先进入原始聊天界面，沿用 DSH 的模型、Session 和审批体验。
- **高级入口：StrongFlow。** 用户主动切换后，可以创建或跟踪 Delivery，分开查看需求和方案，审核系统架构图、流程图、Diff、验收依据和结论。
- **执行图有三种状态。** 执行前节点为绿色；执行中受影响节点为浅蓝色，但不开放具体改动；候选冻结后节点为黄色，可以查看文件和 hunk，并提交精确返工标注。

## 快速开始

这是仓库唯一的首次运行路径。它先运行一个完全无密钥的 Delivery，再启动 DSH Chat。

### 1. 准备环境

需要：

- Node.js 24.x；发布构建固定使用 [`.node-version`](.node-version) 中的 24.19.0；
- Corepack 和 pnpm 11.7.0；
- Rust 1.95.0，包含 `rustfmt`、`clippy` 和 `rust-src`；
- macOS 的 Xcode Command Line Tools，或 Linux 上的 C/C++ 编译器、`pkg-config` 和 libcap 开发文件。

Debian / Ubuntu 可以用以下系统包满足 Linux 原生构建要求：

```bash
sudo apt-get install build-essential pkg-config libcap-dev
```

先确认工具版本：

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

`build` 会编译 TypeScript、当前主机的 Rust 原生模块、内嵌 Codex helper，以及当前平台需要的沙箱组件。安装边界会直接拒绝不受支持的 Node 版本或操作系统。

### 3. 运行无密钥 Delivery

```bash
corepack pnpm fixture:delivery
```

这条命令不读取 API Key，也不访问模型网络。它使用脚本化 DSH 模型响应，但继续运行真实的 DSH Session、内嵌 Codex Core、StrongFlow 服务、本地 Git 候选、语义投影和验收计算。

场景会依次证明：

1. 方案审核开始时，Delivery 为 `needs-attention`，人工 StageRun 为 `waiting`，执行阶段数量为 `0`；
2. 脚本化人工决定先要求修改方案，旧审核决定随后失效；
3. 新方案获批后才进入执行；
4. 第一个候选验证失败，人工确认有限返工；
5. 新候选由独立 Reviewer 和 Verifier 再次验证；
6. 输出逐项 `criterionResults` 和最终 `deliveryVerdict`。

成功结果中的关键字段如下；身份摘要会由实际运行生成：

```json
{
  "kind": "winwincode.keyless-delivery-fixture",
  "finalStatus": "delivered",
  "humanGate": {
    "statusBeforeDecision": "needs-attention",
    "reviewStageStatus": "waiting",
    "executionStageCountBeforeDecision": 0
  },
  "criterionResults": [
    {
      "verdict": "pass"
    }
  ],
  "deliveryVerdict": {
    "status": "pass",
    "unresolvedFindings": []
  },
  "credentialNames": []
}
```

### 4. 启动产品界面

```bash
corepack pnpm start
```

命令会启动 DSH Web，并默认打开 Chat。需要真实模型时，在 DSH 原有设置中选择 Provider、模型和凭据；这些内容不会写入 Delivery。

GitHub 发布凭据也由 DSH 保存。需要运行显式 `live` 发布时，在 DSH 凭据文件 `$DSH_HOME/.credentials.yaml` 中配置 `GITHUB_TOKEN`；StrongFlow 只在每次 GitHub 请求开始时解析该引用，不会把 token 写入 Delivery、审核包、发布 journal 或响应。未选择 `live` 时仍执行零远端写入的 dry-run。

在会话界面主动切换到 **StrongFlow**，即可进入高级工作台。当前页面支持：

- 创建或按 ID 跟踪 Delivery；
- 确认当前 `DeliverySpec`，并用一个“推进下一阶段”操作建立、绑定和驱动当前合法的角色 Session；
- 查看 `DeliverySpec`、范围、约束和验收条件；
- 在独立方案区查看系统架构图、流程图、风险和未决事项；
- 只允许绑定的人工 DSH Session 提交方案或交付决定；
- 查看执行前、执行中和执行结束图；
- 在执行结束图上把返工意见绑定到当前黄色节点和 Diff hunk，并自动交给有次数上限的 `remediator` Session；
- 直接查看绑定 Session 的 Codex Plan、Agent Graph、命令、测试、待处理交互、失败恢复、变更数量和用量，并可继续打开原始 Chat Session。

### 5. 检查发布包体验

```bash
corepack pnpm verify:installed-host
```

该检查会把当前包安装到空目录，启动真实 DSH Web，确认 Chat 默认入口和 StrongFlow 高级入口，运行无密钥角色 Session，并检查 CLI、人工 Attention、信号中断、重启恢复和临时目录清理。成功时会输出：

```text
installed host package passed DSH Web, keyless chat, CLI, signal, restart, Attention, and cleanup smokes for TARGET
```

### 常见问题

- 安装时可能看到其他三个原生平台包的 `Unsupported platform` 警告。工作区同时声明四个平台包；只要当前主机属于支持矩阵且命令继续执行，这些跳过提示不表示当前平台构建失败。
- Linux 原生构建如果报告找不到 `cc`、`pkg-config` 或 `libcap`，先安装本节列出的系统构建依赖，再重新运行构建命令。
- Chat 页面可以无密钥启动；真实模型请求仍需要在 DSH 设置中选择可用 Provider、模型和凭据。
- 端口或自动打开浏览器不符合当前环境时，可以把 DSH Web 参数传给同一个启动命令，例如 `corepack pnpm start web --no-open --port 3000`。

## 当前已经具备的能力

- 直接嵌入固定版本的 Codex Core Rust 源码，不依赖外部编程 Agent 或 Codex CLI 进程；
- 通过 DSH `ctx.llm` 使用 DSH 的模型与 Provider 兼容层；
- 保存 DSH Session 与 Codex Session 的精确绑定和可重放运行事件；
- 保留结构化 Plan、Agent Graph、用户问题、命令、测试、Diff、失败恢复和用量；
- 分开保存需求和方案，方案经过人工审核后才允许执行；
- 支持独立 Reviewer、Verifier 和可选 Adversarial Verifier；
- 从当前 Spec、候选和运行事实计算 Evidence、逐项结果与 Verdict；
- 支持有限返工、旧候选失效、重启恢复和最终人工交付审核；
- 从 StrongFlow 浏览器完成需求确认、方案生成与审核、候选执行、独立验证、黄色节点返工和最终交付审核；
- 生成确定性的 GitHub Review Package，并以 dry-run 作为默认发布模式；
- 随安装 profile 提供使用 DSH `GITHUB_TOKEN` 引用的 GitHub 适配器，可对账 branch、Pull Request、Issue comment 和 commit status；
- 为四个 macOS/Linux 目标生成独立原生包和发布证据；
- 从来源事实派生完整度、可信度、稳定性、人工依赖度和效率，不生成黑盒总分。

## 当前限制

- Windows 尚未进入首发平台。
- 当前运行方式是本机单用户 Host；Organization、共享数据库、RBAC、SSO、多租户隔离和跨机器调度尚未进入这一版本。
- 真实模型执行需要用户在 DSH 中配置可用 Provider。无密钥 fixture 证明流程和边界，不代表某个模型在真实项目中的质量。
- GitHub 远端写入需要显式 live 模式、当前人工批准和 DSH 中已配置的 `GITHUB_TOKEN`；默认流程只生成本地审核包和 dry-run 记录。
- Jira、Linear、Slack 和 Teams 仍是外部协作系统；当前仓库没有这些连接器。

## 开发与验证

贡献代码前先阅读 [参与指南](CONTRIBUTING.md)，用 Beads 领取工作，并按 [Pull Request 模板](.github/pull_request_template.md) 记录实际检查结果。

最常用的完整检查：

```bash
corepack pnpm verify
```

独立检查：

```bash
corepack pnpm format:check
corepack pnpm lint
corepack pnpm typecheck
corepack pnpm test
corepack pnpm build
corepack pnpm verify:packages
corepack pnpm verify:installed-host
corepack pnpm verify:upstream
```

真实模型评估必须显式加入，并使用固定仓库、模型路由和预算。配置与结果说明见 [真实模型与真实仓库评估](docs/live-evaluation.md)。

Codex Core 或 DSH 升级按 [上游更新手册](docs/upstream-updates.md) 单独完成。版本、九个包的发布顺序和回滚见 [发布流程](docs/releasing.md)；四个平台原生产物和当前真实 Delivery 结果的证据细节见 [产品发布门禁](docs/release-gate.md)。该门禁生成审核报告，不执行 npm 发布、Git tag 或 GitHub Release。

## 仓库结构

```text
apps/host/             DSH Web 主机和 winwincode CLI
packages/contracts/    Delivery、运行事件和 StrongFlow API 合同
packages/dsh-profile/  DSH Session、模型回调和 Codex 事件接入
packages/strongflow/   Delivery 服务、证据、Verdict、工作台和 GitHub 边界
packages/native/       Node 到 Rust 的内核接口和平台选择
packages/native-*/     四个平台的单目标原生包
crates/helper/         Codex helper 与平台沙箱入口
crates/kernel/         内嵌 Codex Thread、权限、事件和生命周期
crates/native/         Node 原生模块边界
upstream/              固定上游身份、补丁和许可记录
tests/                 无密钥流程、产品界面、恢复、安全和发布检查
```

## 设计与来源

- [`0.1.0-alpha.1` 发布说明](docs/releases/0.1.0-alpha.1.md)
- [产品架构、交付流程与安全模型](docs/architecture.md)
- [参与指南](CONTRIBUTING.md)
- [上游更新手册](docs/upstream-updates.md)
- [发布流程](docs/releasing.md)
- [安全报告](SECURITY.md)
- [社区行为准则](CODE_OF_CONDUCT.md)
- [固定 Codex Core 与 DSH 集成边界](docs/decisions/0001-upstream-integration.md)
- [TypeScript 表现层、Rust Control Plane 与 Rust Execution Worker](docs/decisions/0028-control-plane-worker-migration.md)
- [Control Plane HTTP 合同](schema/winwincode/v1/control-plane-http.schema.json)
- [Control Plane WebSocket 合同](docs/contracts/control-plane-websocket.md)（[机器可读 schema](schema/winwincode/v1/control-plane-events.schema.json)）
- [ExecutionPort 合同](docs/contracts/execution-port-v1.md)（[机器可读 schema](schema/winwincode/v1/execution-port.schema.json)）
- [Canonical Delivery 所有权](docs/decisions/0023-canonical-delivery-ownership.md)
- [确定性 Delivery fixture](docs/decisions/0024-deterministic-delivery-fixture.md)
- [CPB 设计知识迁移记录](docs/decisions/0022-cpb-design-knowledge-migration.md)

CPB 只作为已核对的设计来源；当前产品使用 WinWinCode 自己的合同、存储和运行路径。

## 许可证

WinWinCode 自有代码只使用 [Apache License 2.0](LICENSE)。Codex、DeepSeek Harness 和其他第三方组件的归属要求记录在 [NOTICE](NOTICE) 和 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 中。
