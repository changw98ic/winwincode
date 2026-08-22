# ADR-0001：固定 Codex Core 与 DeepSeek Harness 集成边界

- 状态：已接受
- 日期：2026-08-21
- 对应任务：`winwincode-9c4.1.1`
- 机器可读清单：[`upstream/sources.lock.json`](../../upstream/sources.lock.json)

## 结论

WinWinCode 只保留一个执行内核：直接嵌入 Codex Core 的 Rust 源码。默认聊天和 StrongFlow 高级模式都创建 Codex 线程、提交 Codex 操作并消费 Codex 事件，不启动外部 Codex CLI，也不启动 Codex app-server 子进程。

DeepSeek Harness（下文简称 DSH）负责 Web/Cordis 界面、原始聊天界面、模型列表、模型设置、凭据和多提供商适配。DSH 的 `agent-loop` 不负责推理、工具调用、沙箱或多 Agent 调度；WinWinCode 实现 DSH 公共 `AgentFactory` 接口，把原始聊天界面的操作接到同一个 Codex 内核。

这使两种界面只有展示和流程控制上的区别，不会形成“普通聊天走 DSH、强流程走 Codex”的两套执行系统。

## 固定的上游源码

| 上游 | 仓库 | 固定版本 | 固定提交 | 本次下载归档 SHA-256 | 许可证 |
| --- | --- | --- | --- | --- | --- |
| OpenAI Codex | `https://github.com/openai/codex` | `rust-v0.149.0` / Cargo `0.149.0` | `758ef40f50c1a458425c7cfbf1eb12cbc07af0b0` | `0413a0e7680bcc2b6c6e998a6ad358115707317ef5d0121dcb9275e88c36121a` | Apache-2.0 |
| DeepSeek Harness | `https://github.com/deepseek-ai/deepseek-harness` | `dsh-v0.1.0-rc.8` / npm `0.1.0-rc.8` | `141eb6fef83422698aef7a981029e843e8161534` | `46fb9d6f103bb7033d066de637069918012a4986aa1f53de793f1f5bdb2d95f1` | MIT |

提交号是源码身份。归档摘要只校验本次取得的 GitHub 归档；若 GitHub 日后重新生成同一提交的归档，应重新记录摘要，但不能改变提交号后仍声称是同一固定版本。

禁止使用 `main`、`latest`、版本范围、未固定的 Git URL 或运行时下载来代替上述提交。

## 运行结构

```text
DSH 原始聊天界面（默认） ─┐
                           ├─ DSH Remote/API + ctx.agents ─ WinWinCode AgentFactory ─┐
StrongFlow 工作台（高级） ─┘                                                        │
                                                                                    ├─ N-API 原生桥 ─ Codex Core
DSH 模型设置/凭据 ─ ctx.llm.prepareCall().stream() ─ WinWinCode 模型流回调 ───────────┘
```

### TypeScript 主机

TypeScript 主机组合 `@deepseek-ai/dsh-base` 和 `@deepseek-ai/dsh-web-app`，再应用 WinWinCode 覆盖层：

1. 保留 Cordis、Web 服务、原始聊天界面、会话投影、设置、凭据、`ctx.llm` 和 `llm-pi-ai`。
2. 保留 `@deepseek-ai/dsh-agent` 的 `ctx.agents` 注册表和对外 Remote/API 合同。
3. 禁用 DSH `agent-loop` 以及 DSH 自己的模型驱动、工具执行、子 Agent 提供方、工作流执行和沙箱执行入口，避免第二个执行权威。保留 API 网关硬性依赖的 `tools`、`subagent` 注册表，以及 WinWinCode 工厂用于模型选择和人工审批的 `system-prompt`、`approval` 服务；这些保留项不执行 DSH agent loop。
4. 注册 WinWinCode 的 `AgentFactory`。它实现 `createAgent` 和 `resume`，对外提供 DSH `Agent` 所需的发送、转向、取消、空闲等待和会话投影行为，对内全部映射到 Codex 线程。
5. 默认路由仍是 DSH 的 `ui-conversation`。StrongFlow 作为新的 UI 插件和 API 命名空间加入，不替换默认聊天入口。

DSH 的 bundle patch 是整行替换，不是深度合并。因此 WinWinCode 覆盖层修改配置时必须重述该行的完整 `config`，并用测试检查被禁用的执行行没有因上游调整而重新启用。

### Rust 内核

WinWinCode 自有的 `winwincode-native` `cdylib` 通过 N-API 嵌入 Node 进程。会话生命周期只从 `codex-core-api` 公共门面进入内核；事件封装直接使用公开的 `codex-protocol` 类型，以免复制或缩减工具和子 Agent 事件。主要使用：

- `ThreadManager`、`StartThreadOptions`、`NewThread`：创建和恢复线程；
- `CodexThread`：持有线程；
- `TurnInput`、`TurnInputRequest`、`TurnStartOptions`：提交用户输入；
- `Op`：提交转向、审批、取消和其他操作；
- `EventMsg`：向 DSH 聊天界面和 StrongFlow 投影运行事件；
- `SessionSource`、配置、权限、状态库、线程存储和 Agent 图存储接口。

`upstream/sources.lock.json` 列出从 `codex-core-api` 出发、排除开发依赖后的全部本地 Cargo 包闭包。外部 Rust 依赖由固定提交中的 `codex-rs/Cargo.lock` 唯一确定。不得手工复制一份容易失真的外部依赖版本表。

### 模型兼容边界

模型列表、提供商配置和密钥仍由 DSH 管理。每次 Codex 模型调用通过桥接回调执行：

1. TypeScript 调用 `await ctx.llm.prepareCall(config, signal)`；
2. 使用返回对象的 `prepared.config` 组装 `GenerateOptions`；
3. 只调用一次 `prepared.stream(options)`；
4. 将 `block-start`、文本、推理、工具调用、`block-end`、用量和结束原因映射为 Codex `ResponseEvent`；
5. 将取消、提供商错误、请求编号和重试信息映射回 Codex 的错误和事件语义。

选择 `prepareCall` 而不是先查模型再单独调用 `ctx.llm.stream`，是为了把一次调用固定到同一个 DSH adapter 注册实例，避免热更新期间能力信息和实际 adapter 不一致。

Codex `0.149.0` 的 `ModelClient` 会自行创建 `HttpClientFactory` 和 `ReqwestTransport`，公开的 `ModelProvider` 只管理模型元数据、认证和目录，并不接管推理流。因此需要一个很小的 Codex 补丁，在模型会话边界注入提供商无关的推理流接口；不使用本地 HTTP 回环服务伪装模型提供商。

## 被消费的 DSH 表面

完整的本地 workspace 包闭包记录在锁文件中，由以下根计算：

- 运行根：`@deepseek-ai/dsh-base`、`@deepseek-ai/dsh-web-app`、`@deepseek-ai/cordis`；
- Web 构建根：`@deepseek-ai/dsh-web-frontend`，包括该根的构建依赖；
- 外部 npm 闭包：固定提交中的 `pnpm-lock.yaml`。

必须稳定检查的公共边界为：

| 能力 | 包 / 服务 | 使用方式 |
| --- | --- | --- |
| Cordis 组合 | `@deepseek-ai/cordis`、`dsh-base`、`dsh-web-app` | 组合 stock Web profile 和 WinWinCode 覆盖层 |
| 默认聊天 | `@deepseek-ai/dsh-client-ui-conversation`，行 `ui-conversation` | 保持为默认页面 |
| 浏览器外壳 | `modules`、`connection`、`api-remotes`、`client-runtime`、`ui-layout`、`ui-renderer`、`ui-sidebar` | 装载原始界面和 StrongFlow 扩展 |
| 主机 API | `typert`、`typert-loader`、`typert-gateway`、`api-gateway`、`webserver` | 浏览器到主机的类型化调用和事件流 |
| Agent 兼容 | `@deepseek-ai/dsh-agent` 的 `AgentFactory`、`Agent`、`ctx.agents` | WinWinCode 工厂替代 DSH loop，但保持 UI/API 合同 |
| 会话展示 | `@deepseek-ai/dsh-session`、会话持久化和 projection 行 | 将 Codex 事件投影为可从内核记录重建的 DSH 展示副本 |
| 模型运行时 | `@deepseek-ai/dsh-llm` 的 `ctx.llm`、`prepareCall`、`PreparedLlmCall.stream` | 模型目录和流式调用 |
| 多模型适配 | `@deepseek-ai/dsh-llm-pi-ai` | 使用 DSH 的提供商兼容层 |
| 设置 | `@deepseek-ai/dsh-settings`、`@deepseek-ai/dsh-settings-file` | 保存和热更新模型设置 |
| 凭据 | `@deepseek-ai/dsh-credentials`、`@deepseek-ai/dsh-credentials-local` | 按请求解析密钥，不写入普通设置 |
| 模型界面 | `ui-settings`、`ui-settings-models`、`ui-model-selection` | 继续使用 DSH 原始模型配置和选择界面 |

锁文件同时记录 stock `dsh-base` 和 `dsh-web-app` 的全部 Cordis 行名。升级检查会重新读取两个 `cordis.patch.yml`；上游新增、删除或改名时必须人工审查覆盖层，而不是静默接受。

## 明确的补丁集

对 vendored 上游源码的补丁只放在 `upstream/patches/`，按固定顺序应用；业务功能不得散落修改 vendored 上游文件。WinWinCode 自有的 DSH 组合层不是上游源码补丁，其唯一运行副本是发布包内的 `packages/dsh-profile/cordis.patch.yml`，锁文件直接记录这一路径，不再维护一份重复的 diff。

| 补丁 | 上游位置 | 目的 |
| --- | --- | --- |
| `codex/0001-export-client-mcp-extensions.patch` | `codex-rs/core-api/src/lib.rs` | 从既有公共协议模块重导出 `ClientMcpExtensions`，使只依赖 `codex-core-api` 的嵌入方能调用公开的 resume 方法 |
| `codex/0002-inject-model-stream-transport.patch` | `codex-rs/core/src/client.rs`、会话/线程管理和 `codex-rs/core-api/src/lib.rs` | 注入一个窄的异步模型流接口，并让根线程、恢复会话、分叉和子 Agent 共用该接口；未注入时仍走上游原有 HTTP 路径 |
| `codex/0003-export-config-builder.patch` | `codex-rs/core-api/src/lib.rs` | 重导出上游已有的 `ConfigBuilder`，让嵌入内核读取自己数据目录中的 `config.toml`，而不是退回到不读取配置文件的默认只读设置 |
| `codex/0004-resume-with-caller-options.patch` | `codex-rs/core/src/thread_manager.rs` | 恢复 rollout 时保留调用方已经固定的动态工具、环境选择和扩展设置，使角色恢复不能重新获得默认工具或丢失原有权限边界 |
| `packages/dsh-profile/cordis.patch.yml` | 不修改 DSH 源文件；作为 WinWinCode 自有 bundle 的最终组合层 | 禁用 DSH 执行行并装载 Codex AgentFactory、模型桥和 StrongFlow UI/API |

升级时每个补丁必须能以零模糊匹配应用；失败即表示集成边界变化，必须重新审查。

## 已知不稳定边界

DSH 当前是 `0.1.0-rc.8` 开发预览，明确允许破坏性变更。以下表面全部视为不稳定：

- Cordis bundle 行名及“整行替换”行为；
- `AgentFactory`、`Agent`、会话事件、Remote/API 和 UI 插件槽位；
- `ctx.llm` 的 `GenerateOptions`、`PreparedLlmCall`、`StreamChunk`；
- 设置和凭据命名空间；
- 原始聊天界面的内部组件结构。WinWinCode 只能依赖公开插件和 Remote 表面，不能导入其私有 React 组件路径。

Codex 的 `codex-core-api` 是首选门面，但 `ModelClient` 注入点和内部 `ResponseEvent` 映射仍属于补丁边界。任何上游升级都必须重新运行合同测试和补丁检查，不能按语义版本推断兼容。

## 平台和构建要求

首个版本只构建以下四个目标：

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-gnu`

固定工具要求：Rust `1.95.0`（含 `rustfmt`、`clippy`、`rust-src`）、Node.js `24.x`、pnpm `11.7.0`。DSH 上游允许 Node `^22.19.0 || >=24.0.0`，WinWinCode 收窄到 Node 24，减少发布组合。Linux GNU 包需要目标架构对应的 C/C++ 链接工具；macOS 通用发布由两个 Darwin 目标分别构建，不把交叉编译结果冒充已验证产物。

其他平台必须在安装或启动阶段给出清楚的“不支持平台”错误，不能回退到外部 Codex CLI。

## 许可证和通知

WinWinCode 项目许可证只使用 `Apache-2.0`。根 `LICENSE`、包元数据和发布物不得写成 `Apache-2.0 OR MIT`，也不得把 DSH 的 MIT 许可证声明为 WinWinCode 的第二项目许可证。

源代码或二进制发布物必须同时：

1. 带上 WinWinCode 的 Apache-2.0 `LICENSE`；
2. 保留 Codex 的 Apache-2.0 许可证；
3. 原样保留 Codex `NOTICE` 中的 OpenAI Codex、Ratatui 和版权通知，并在修改文件上保留显著修改说明；
4. 在第三方通知中保留 DSH 的 MIT 完整许可文字和 `Copyright (c) 2026 DeepSeek`；
5. 保留并重新生成 DSH `THIRD_PARTY_NOTICES.md` 所代表的实际发布依赖通知；
6. 保留 vendored Cordis 及其他 vendored 组件自己的许可证；
7. 不使用 DeepSeek 或 OpenAI 商标暗示 WinWinCode 是其官方产品。

MIT 通知是第三方归属义务，不改变 WinWinCode 自有代码的 Apache-2.0 许可。

## 不迁移的内容

- 不迁移 CPB 的任务、队列、日志、数据库、运行记录、用户配置或任何内部数据。
- 只把已经确认的 CPB 设计原则重新实现为 WinWinCode 的新合同；不复制 CPB 运行时。
- 不引入旧版兼容路径。
- 不依赖已安装的编程 Agent、Codex CLI、DSH CLI Agent loop 或额外 app-server 进程完成执行。

## 可重复升级检查

仓库内置的 Codex 源码、元数据、许可证、补丁和公共门面默认执行：

```bash
node scripts/verify-upstream-lock.mjs
```

检查新的上游候选目录时执行：

```bash
node scripts/verify-upstream-lock.mjs \
  --codex-root /path/to/codex-at-758ef40f \
  --dsh-root /path/to/deepseek-harness-at-141eb6fe
```

检查必须验证：

- 提交、标签、版本和许可证没有浮动值；
- Codex Cargo 版本、Rust 工具链、`NOTICE`、公开门面和补丁锚点仍存在；
- 从 `codex-core-api` 计算的非开发本地 crate 闭包与锁文件完全相同；
- DSH Node/pnpm 要求、MIT 许可、第三方通知和接口文件仍存在；
- 从选定 DSH 根计算的 workspace 包闭包与锁文件完全相同；
- 两个 stock Cordis profile 的全部行名与锁文件完全相同；
- 四个发布目标、补丁路径和许可证义务没有丢失。

升级流程为：下载候选提交到临时目录、更新一个上游固定项、重新生成并检查闭包、应用补丁、运行上述校验、运行 native/聊天/StrongFlow 合同测试、人工查看源码差异和通知差异，最后才更新锁文件。任何一步失败都停止升级。
