# WinWinCode

WinWinCode 是一个面向 macOS 和 Linux 的开源 Agent 工作台。默认入口沿用 DeepSeek Harness 的原始聊天界面；StrongFlow 是高级模式。两种模式共用一个直接嵌入的 Codex Core 执行内核。

当前仓库处于基础内核和界面组合阶段。Codex Core 已作为 Rust 源码直接编入 Node 原生模块，TypeScript 可以创建、恢复、分叉、转向、打断和关闭会话，也可以读取有上限且有顺序的完整 Codex 事件。Codex 的模型流已经直接接到 DSH `ctx.llm`，密钥仍只由 DSH 管理。内核事件会生成可重放的 DSH 聊天和运行状态，人工审批会返回原始 Codex 回调。DSH 原始聊天入口、WinWinCode 内核工厂和 StrongFlow 高级入口已经组成一个可启动的 DSH Web profile。StrongFlow 已固定八个角色，并能在完整上下文安装、保存和事件订阅成功后发布各自的受管内核会话。角色运行器现在可以提交一个准确的内核回合，限制回合、时间、Token、费用和输入字节数，记录事件范围，并且只有角色所欠的全部结构化制品都通过校验才返回成功。RequirementSpec、SolutionDesign、两张定义图、人工审核、执行计划、代码变更、审查、验证、修复、交付和图上变更标注现在都有唯一的严格格式；模型只能填写内容，身份和内核证据由程序加入。每份方案现在还能确定生成系统架构图和固定流程图，以及不含脚本和外部资源的 Mermaid、SVG 和内容摘要；未确认信息会明确显示而不会被猜测补全。通过校验的制品、命令输出和模型观察现在写入内容寻址存储：正文按摘要去重，每个作业只读取自己的连续元数据记录，读取时重新核对大小、摘要、生产者、内核事件范围和候选版本。人工审核和八个角色的输入现在由程序按当前流程状态选择，并固定到具体制品记录和正文摘要；需求角色看不到方案，方案角色只看到需求，执行角色只看到已批准定义产生的执行计划，之后的审查、验证和修复始终绑定同一个冻结候选版本。已发布的交接可在流程继续后从本地记录原样重建。DSH 高级工作台和 CLI 现在共用一个可重启的本地作业服务：创建、状态、事件跟随、分开的需求与方案读取、两张图读取、三种人工决定、取消、恢复、制品列表和导出都使用同一个版本化信封；同一变更请求重试会返回第一次结果，过期审核不能解锁执行，Remote 或 CLI 断开也不会取消作业。DSH Host Remote 和 `winwincode` CLI 已接入该服务，响应必须匹配原请求、作业和审核定义，运行中的差异事件明确禁止细节访问。篡改正文、交换来源、旧批准、外来内核会话、重复结果、旧候选版本、证据丢失和中断发布现在都有失败关闭的回归测试；错误只报告稳定代码和核对位置，不复制受保护正文。实际工具权限、StrongFlow 完整工作台、图的三种执行状态和完整交付流仍在后续任务中，当前还不是可交付的完整应用。

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

`build:native` 会编译当前主机对应的 Rust 原生库和内部 helper，并放到一个独立的平台包，例如 Apple Silicon 使用 `packages/native-darwin-arm64/prebuild/`。`@winwincode/native` 只负责按主机选择四个平台包之一，不会把四套大体积二进制塞进同一个下载。这个 helper 只处理 Codex 的沙箱、文件系统和子进程入口，不是外部 Codex CLI。

每个平台包都带 `build-info.json`、SHA-256、固定源码身份、构建工具版本、`LICENSE`、`NOTICE`、`THIRD_PARTY_NOTICES.md` 和该目标实际 Cargo 依赖的许可清单。`verify:native-install` 会从打包文件安装到一个空目录，再运行不使用密钥的内核工具调用，并确认沙箱允许工作区写入、阻止越界写入。

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
