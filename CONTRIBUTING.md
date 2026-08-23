# 参与 WinWinCode

感谢你改进 WinWinCode。本仓库同时包含 TypeScript 产品层和 Rust 执行边界。提交前请先阅读 [产品架构、交付流程与安全模型](docs/architecture.md)：Codex Core 负责执行，DeepSeek Harness（DSH）负责产品外壳，WinWinCode 负责交付目标、跨 Session 阶段、业务 Attention、Evidence 和 Verdict。

## 准备开发环境

首发开发环境是 macOS 或 GNU Linux，需要 Node.js 24、pnpm 11.7.0 和 Rust 1.95.0。完整系统依赖见 [README 快速开始](README.md#快速开始)。

```bash
corepack pnpm install --frozen-lockfile
corepack pnpm build
corepack pnpm fixture:delivery
```

无密钥 Delivery 成功结束并输出 `finalStatus: "delivered"` 后，说明本机的 DSH Session、内嵌 Codex Core、StrongFlow 阶段控制和验收计算可以一起运行。

## 领取工作

仓库使用 Beads 保存需求、依赖和进度。开始修改前运行：

```bash
bd prime
bd ready
bd show ISSUE_ID
bd update ISSUE_ID --claim
```

一个改动对应一个已有 Beads issue。实施中发现独立后续工作时，用 `bd create` 建立新 issue，并用 `bd dep add` 记录依赖；不要用 Markdown 文件维护另一份任务清单。只有验收条件已经满足并完成相应检查时，才运行：

```bash
bd close ISSUE_ID --reason="Completed and verified"
```

## 代码位置

| 目录 | 内容 |
| --- | --- |
| `apps/host/` | DSH Web 主机和 `winwincode` 命令 |
| `packages/contracts/` | Delivery、运行事件和 StrongFlow API 数据结构 |
| `packages/dsh-profile/` | DSH Session、模型调用和 Codex 事件接入 |
| `packages/strongflow/` | Delivery 服务、阶段控制、Evidence、Verdict 和工作台 |
| `packages/native/`、`packages/native-*` | Node 到 Rust 的接口和四个平台包 |
| `crates/` | 内嵌 Codex Core、helper 和 Node 原生模块 |
| `upstream/`、`third_party/codex/` | 固定上游身份、补丁、许可记录和 Codex 源码 |
| `tests/` | 无密钥流程、恢复、安全、界面、打包和发布检查 |

TypeScript 使用严格模式和 ESM，缩进两个空格；Rust 使用工作区固定的 `rustfmt` 和 Clippy 配置。行为变化要修改生产代码并添加聚焦测试，不通过修改 fixture、快照或替身来掩盖产品结果。

## 验证改动

开发时先运行最小相关检查，再运行完整检查：

```bash
corepack pnpm format:check
corepack pnpm typecheck
corepack pnpm test
corepack pnpm lint
corepack pnpm build
corepack pnpm verify
```

`corepack pnpm verify` 会再次检查格式、类型、Rust Clippy、测试、CPB 设计边界、包内容、当前平台原生安装、真实 DSH Host、上游固定项和干净目录安装。平台发布证据另按 [发布流程](docs/releasing.md) 生成。

## 修改 Delivery 数据结构

当前源码预览使用 `DELIVERY_SCHEMA_VERSION = 3`。在首个公开稳定版本前，开发数据可以从无密钥 fixture 重新生成，不把早期开发结构作为长期输入。

公开版本发布后，每次 Delivery 数据结构升级采用一条迁移路径：

1. 新版本提供一个离线、一次性迁移程序，只接收上一受支持版本并输出当前版本；
2. 先复制数据目录，在副本上执行迁移并验证 Delivery、SessionBinding、Evidence 和 Verdict 数量与引用；
3. 全部验证通过后再以一次替换启用新数据；原副本保留为回滚点；
4. 新运行时只读取当前结构，旧结构会返回明确的版本错误；
5. 发布材料写明输入版本、输出版本、命令、验证结果和回滚目录。

迁移完成后，产品中保留一个当前读取路径和一个当前写入路径。版本之间不保留双读、双写、静默回退或长期适配器。

## 更新 Codex 或 DSH

每次只更新一个上游来源。候选源码、固定提交、闭包、补丁、DSH profile、许可通知、检查命令和回滚点见 [上游更新手册](docs/upstream-updates.md)。上游身份或行为发生变化时，必须同时更新 `upstream/sources.lock.json` 和直接受影响的测试、通知或决策记录。

## 准备 Pull Request

提交前完成以下内容：

- Beads issue 的目标和验收条件与改动一致；
- 生产行为、测试和文档使用同一个当前名称与数据结构；
- 需求或方案变化会使旧人工决定失效；
- 新 Evidence 可以追溯到命令、测试、Diff、文件、提交、运行事件或独立审查发现；
- 没有密钥、凭据、本地运行状态、日志、依赖目录或构建产物；
- WinWinCode 自有文件继续声明 Apache-2.0；必要第三方文本保存在 `NOTICE` 和 `THIRD_PARTY_NOTICES.md`；
- 已记录实际运行的检查和仍未覆盖的风险。

使用 [Pull Request 模板](.github/pull_request_template.md) 写清交付目标、改动、验证、数据结构或上游影响。安全问题请按 [安全报告流程](SECURITY.md) 私下提交。

## 行为准则与许可

参与讨论和评审即表示同意遵守 [行为准则](CODE_OF_CONDUCT.md)。提交到本仓库的项目代码和文档按 [Apache License 2.0](LICENSE) 提供；提交者应确认自己有权贡献相关内容。
