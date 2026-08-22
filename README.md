# WinWinCode

WinWinCode 是一个面向 macOS 和 Linux 的开源 Agent 工作台。默认入口沿用 DeepSeek Harness 的原始聊天界面；StrongFlow 是高级模式。两种模式共用一个直接嵌入的 Codex Core 执行内核。

当前仓库处于基础内核和界面组合阶段。Codex Core 已作为 Rust 源码直接编入 Node 原生模块，TypeScript 可以创建、恢复、分叉、转向、打断和关闭会话，也可以读取有上限且有顺序的完整 Codex 事件。Codex 的模型流已经直接接到 DSH `ctx.llm`，密钥仍只由 DSH 管理。内核事件会生成可重放的 DSH 聊天和运行状态，人工审批会返回原始 Codex 回调。DSH 原始聊天入口、WinWinCode 内核工厂和 StrongFlow 高级入口已经组成一个可启动的 DSH Web profile。StrongFlow 已固定八个角色，并能在完整上下文安装、保存和事件订阅成功后发布各自的受管内核会话。角色启动前会把固定权限预设、工作区、系统指令和准确工具名单交给 Codex；启动后再核对 Codex 实际保留的文件模式、网络、环境、人工决定和指令来源，证据缺失或不同就关闭线程且不发布会话。模型只能看到自己角色的工具；宿主拒绝名单外调用、敏感凭据路径和越过分配根目录的读写。`command.run` 与 `test.run` 还必须匹配可信流程给出的准确授权，之后只通过 macOS Seatbelt 或 Linux seccomp/Landlock 运行；进程环境从空白开始，网络保持关闭，超时、取消和会话关闭会终止进程组。普通聊天会话不能调用这一入口。DSH 模型桥不读取原始密钥，并会拒绝带凭据字段的调用配置、隐藏提供商错误文字和请求编号原值。工具、审批、进程、会话与凭据边界现在写入按作业隔离的追加式安全记录；记录带连续摘要链，命令输出只保存摘要和字节数。四个平台的发布工作流会从空目录安装真实包，再验证文件、网络、环境、凭据、超时和只读边界。角色运行器可以提交一个准确的内核回合，限制回合、时间、Token、费用和输入字节数，并把流程权限拒绝、操作系统沙箱拒绝和普通任务失败分开记录。RequirementSpec、SolutionDesign、两张定义图、人工审核、执行计划、代码变更、审查、验证、修复、交付和图上变更标注已有唯一严格格式；模型只能填写内容，身份和内核证据由程序加入。方案可以确定生成系统架构图和固定流程图，以及不含脚本和外部资源的 Mermaid、SVG 和内容摘要；未确认信息会明确显示。通过校验的制品、命令证据和模型观察写入内容寻址存储，并按作业、来源、内核事件范围和候选版本重新核对。人工审核和八个角色的输入由程序按当前流程状态选择，需求与方案分开，执行、审查、验证和修复始终绑定准确版本。DSH 高级工作台和 CLI 共用一个可重启的本地作业服务；同一变更请求重试会返回第一次结果，过期审核不能解锁执行，运行中的差异事件禁止读取细节。StrongFlow 完整工作台、图的三种执行状态和完整八角色交付流仍在后续任务中，当前还不是可交付的完整应用。

## 已确定的边界

- 默认界面：DSH 原始聊天界面。
- 高级界面：StrongFlow 工作台。
- 执行内核：直接嵌入 Codex Core Rust 源码，不依赖外部编程 Agent 或 Codex CLI 进程。
- 模型兼容：DSH `ctx.llm`、`llm-pi-ai`、设置和凭据服务。
- 首发平台：Apple Silicon / Intel macOS，arm64 / x64 GNU Linux。
- 项目许可证：Apache-2.0。上游 MIT 内容只作为第三方通知保留，不形成项目双许可证。
- CPB：只迁移设计知识，不迁移运行数据。

固定的上游版本、组件闭包、补丁边界与许可义务见 [`docs/decisions/0001-upstream-integration.md`](docs/decisions/0001-upstream-integration.md)。

## 开发环境

- Node.js 24.x；可发布原生产物固定使用 `.node-version` 中的 24.19.0
- pnpm 11.7.0
- Rust 1.95.0，含 `rustfmt`、`clippy`、`rust-src`

```bash
corepack pnpm install --frozen-lockfile
corepack pnpm verify
```

常用独立命令：

```bash
corepack pnpm typecheck
corepack pnpm test
corepack pnpm lint
corepack pnpm build
corepack pnpm build:native
corepack pnpm verify:packages
node scripts/verify-upstream-lock.mjs
```

`build:native` 会编译当前主机对应的 Rust 原生库和内部 helper，并放到一个独立的平台包，例如 Apple Silicon 使用 `packages/native-darwin-arm64/prebuild/`。`@winwincode/native` 只负责按主机选择四个平台包之一，不会把四套大体积二进制塞进同一个下载。这个 helper 处理 Codex 和 StrongFlow 的沙箱、文件系统和子进程入口，不是外部 Codex CLI。

每个平台包都带 `build-info.json`、SHA-256、固定源码身份、构建工具版本、`LICENSE`、`NOTICE`、`THIRD_PARTY_NOTICES.md` 和该目标实际 Cargo 依赖的许可清单。`verify:native-install` 会从打包文件安装到一个空目录，再运行不使用密钥的内核工具调用，并确认受管命令只使用目标平台沙箱、允许规定的工作区写入、阻止越界和只读写入、拒绝敏感文件与网络、排除宿主密钥环境并执行超时。

## 目录

```text
apps/host/             可发布的 ESM 主机和 CLI
packages/contracts/    跨层 TypeScript 合同
packages/dsh-profile/  DSH Web/Cordis 组合边界
packages/strongflow/   StrongFlow 控制器边界
packages/native/       TypeScript 内核接口、原生模块加载和平台选择
packages/native-*/     四个只含单一操作系统和处理器产物的平台包
crates/helper/         内嵌 Codex 子进程与沙箱 helper
crates/kernel/         Codex 会话、事件、错误和关闭所有权边界
crates/native/         Node 与 Rust 的原生库边界
upstream/              固定上游清单和补丁记录
tests/                 真实构建产物的 Node smoke 测试
```

不支持的平台或 Node 版本会在安装和启动边界给出明确错误，不会回退到另一套执行内核。

## 许可证

WinWinCode 自有代码以 [Apache License 2.0](LICENSE) 发布。Codex、DeepSeek Harness 和其他第三方组件的归属要求记录在 [NOTICE](NOTICE) 和 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 中；平台包还附带目标专属的 Rust 依赖与原始许可文件清单。
