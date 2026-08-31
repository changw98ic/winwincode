# ADR-0001：固定 Codex Core 与上游补丁边界

- 状态：已接受并按阶段 6.7 更新
- 日期：2026-08-21
- 当前边界更新：2026-08-31
- 对应任务：`winwincode-9c4.1.1`、`winwincode-9c4.16.6.7.9`
- 机器可读清单：[`upstream/sources.lock.json`](../../upstream/sources.lock.json)

## 结论

WinWinCode 只保留一条当前产品路径：`apps/client` 通过生成的 HTTP/WebSocket Client 连接 Rust `winwincode-server`；Server 组合 `winwincode-control-plane`，ExecutionPort 连接 `winwincode-worker`，Worker 通过 `winwincode-codex` 使用直接嵌入的 Codex Core，并随附 `winwincode-kernel-helper`。`winwincode-local` 只负责同进程组合，不产生第四个发布二进制。

Chat 是 `apps/client` 的默认页面，StrongFlow 是同一 Client 中的高级页面。两者使用同一 Server、Control Plane、Worker 和 Kernel，不启动已安装的 Codex CLI 或其他编程 Agent，也不经过另一个 Node Host、Cordis 服务或 N-API 桥。

## 当前上游源码

| 上游 | 固定版本 | 固定提交 | 取得的归档 SHA-256 | 当前使用方式 | 许可证 |
| --- | --- | --- | --- | --- | --- |
| OpenAI Codex | `rust-v0.149.0` / Cargo `0.149.0` | `758ef40f50c1a458425c7cfbf1eb12cbc07af0b0` | `0413a0e7680bcc2b6c6e998a6ad358115707317ef5d0121dcb9275e88c36121a` | `third_party/codex/` 中的 Rust 源码，由 Kernel 直接构建 | Apache-2.0 |
| `i18n-embed-fl` | Cargo `0.9.4` | `ceb3da0ee3acf91b17a7a52e02642267ddb47a3d` | crates.io checksum `04b2969d0b3fc6143776c535184c19722032b43e6a642d710fa3f88faec53c2d` | `upstream/vendor/i18n-embed-fl-0.9.4/` 中的精确 vendored source | MIT |

完整提交号或 registry checksum 是源码身份。不得使用 `main`、`latest`、版本范围、未固定 Git URL 或运行时下载代替这些身份。

## 当前运行结构

```text
apps/client (Chat + StrongFlow)
          │ generated HTTP / WebSocket client
          ▼
winwincode-server
          ├─ winwincode-control-plane
          │          │ ExecutionPort
          │          ▼
          └─ winwincode-worker
                     ├─ winwincode-codex → winwincode-kernel → Codex Core
                     └─ winwincode-kernel-helper

winwincode-local 只组合 Server、Control Plane 和 Worker。
```

Codex 的 Thread、Turn、Plan、Agent Graph、工具、Shell、沙箱、权限、Diff、用量和恢复事实只由 Kernel 持有。Control Plane 只持有 ProductSession、Delivery、Provider、Credential 引用、策略、调度、Publication 和 Audit。Worker 不接触长期 Provider 凭据，Client 不直连 Worker。

## Codex 公共边界

`codex-core-api` 是 WinWinCode 进入 Codex Core 的公共门面。当前固定并由依赖图使用的主要符号包括：

- `ThreadManager`、`StartThreadOptions`、`NewThread`；
- `CodexThread`、`TurnInput`、`Op`、`EventMsg`；
- `ConfigBuilder` 和 `SessionSource`。

机器可读清单记录实际生产 Cargo 闭包、所需接口和公共符号。外部 Rust 依赖由根 `Cargo.lock` 和固定 Codex 源码共同锁定，不另维护一份手写版本表。

模型请求通过 Kernel 注入的模型流接口进入 Control Plane Provider Gateway。长期 Credential 只在 Server/Control Plane 边界解析；Worker 与 Codex Core 只收到当前请求需要的受限输入和流式结果。

## 明确的补丁集

上游补丁只放在 `upstream/patches/`，按 `upstream/sources.lock.json` 的固定顺序审计。业务功能不得散落修改 vendored 上游文件。

| 补丁 | 上游位置 | 目的 |
| --- | --- | --- |
| `i18n-embed-fl/0001-stable-specified-argument-order.patch` | `upstream/vendor/i18n-embed-fl-0.9.4/src/lib.rs` | 回移上游 `f02d3ca8` 的可复现构建修复：编译期宏参数按 key 稳定排序，生成的运行时 Fluent 参数仍使用原有 `HashMap` |
| `codex/0001-export-client-mcp-extensions.patch` | `codex-rs/core-api/src/lib.rs` | 从公共门面导出已有 MCP extension 类型 |
| `codex/0002-inject-model-stream-transport.patch` | `codex-rs/core` 与 `core-api` | 注入 Provider Gateway 使用的模型流接口 |
| `codex/0003-export-config-builder.patch` | `codex-rs/core-api/src/lib.rs` | 导出上游已有 `ConfigBuilder` |
| `codex/0005-remount-split-bwrap-root-read-only.patch` | `codex-rs/linux-sandbox/src/bwrap.rs` | 完成批准挂载后把合成根重新挂为只读 |
| `codex/0006-tool-gate-and-exact-turn-replay.patch` | `codex-rs/core` 及对应 lock/module 文件 | 固定工具调用门禁和精确 Turn replay |

每个补丁必须能对记录的上游身份精确应用。补丁失败表示上游边界已经改变，需要重新审查，不能放宽断言或保留第二条旧路径。

## 可复现构建补丁

`age 0.11.2` 间接使用 `i18n-embed-fl 0.9.4`。该版本原始 proc macro 会遍历随机种子的 `HashMap`，使同一宏输入生成不同顺序的 LLVM IR。仓库保留 crates.io `0.9.4` 的完整源码与 MIT 许可证，只把编译期 `specified_args` 改成排序后的 `Vec`；宏生成的运行时 `HashMap` API 不变。

`Cargo.toml` 的 `[patch.crates-io]` 只选择这个 path source，`Cargo.lock` 中只能有一个 `i18n-embed-fl 0.9.4` 且没有 registry source/checksum。原始归档、原始与补丁后源码树、补丁、许可证和上游修复提交的 SHA-256 都记录在机器可读清单中。

## 历史来源记录

DeepSeek Harness `0.1.0-rc.8` 曾作为阶段 6.6 之前的界面与集成评估来源。阶段 6.6 已把当前产品收敛到项目自有 Client 和 Rust Server/Worker/Local 路径；当前 pnpm workspace、Cargo workspace、发布产物和运行时均不包含 DeepSeek Harness 或 Cordis 包。

机器可读清单只保留该候选的仓库、tag、commit、archive SHA-256 和 MIT 许可证，状态为 `historical-attribution-only`，并明确它不是当前 workspace 依赖或分发内容。对应 MIT 通知继续保留在 `THIRD_PARTY_NOTICES.md`，但不构成当前产品入口、升级合同或第二项目许可证。

## 平台与工具

首个版本只构建：

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-gnu`

固定工具为 Rust `1.95.0`、Node.js `24.x` 和 pnpm `11.7.0`。Linux GNU runner 安装目标架构需要的 C/C++ 链接工具和 libcap 开发文件；macOS 两个 Darwin target 分别在原生 runner 构建。

## 许可证与通知

WinWinCode 自有代码只使用 Apache-2.0。源代码与平台产物必须：

1. 带上 WinWinCode 的 `LICENSE`、`NOTICE` 和 `THIRD_PARTY_NOTICES.md`；
2. 保留 Codex 的 Apache-2.0 许可证和 NOTICE 义务；
3. 保留 vendored `i18n-embed-fl` 的 MIT 许可证；
4. 保留历史评估来源依法需要的归属文字，同时明确它不是当前运行依赖；
5. 不把第三方 MIT 条款写成 WinWinCode 的第二项目许可证。

## 更新与回滚

当前升级流程见[上游更新手册](../upstream-updates.md)。每次只更新 Codex 或一个 vendored Cargo source；候选目录放在仓库外，固定完整身份，建立修改前、替换后、完整验证后三个回滚点。更新后至少运行：

```bash
cargo metadata --locked --offline --format-version 1
node --test tests/i18n-embed-fl-reproducibility.test.mjs tests/open-source-governance.test.mjs
corepack pnpm verify
```

任何源码、补丁、Cargo lock、许可证、通知或发布输入发生变化，旧的构建与发布证据全部失效，必须在新的 release source digest 上重新生成。
