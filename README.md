# WinWinCode

WinWinCode 是可自托管的 AI 编程工具。你在浏览器中提交任务，连接的设备运行内嵌 Codex 内核，执行代码修改和检查。

- **Chat**：连续对话，提交编程任务，查看模型和工具的执行记录。
- **StrongFlow**：将需求、方案、执行、评审和验证组织成交付流程。
- **候选改动**：查看任务产生的代码差异、验证证据和交付状态。

## 安装与使用

从 [GitHub Releases](https://github.com/changw98ic/winwincode/releases) 下载发布包。支持 macOS 和 GNU Linux 的 ARM64、x64。

按 [部署指南](docs/operations/deploy.md) 完成以下步骤：

1. 配置并启动 Server 和浏览器界面。
2. 在执行设备上启动 Device Client，绑定本地仓库。
3. 在浏览器中连接设备、配置模型，然后使用 Chat 或 StrongFlow 提交任务。

0.2.0 的新库 password 初始化流程尚未接通。首次部署请使用指南中的本机回环 `local-open` 配置。

## 从源码构建

需要 Node.js 24.19.0、pnpm 11.7.0、Corepack 和 Rust 1.95.0。macOS 和 Linux 的编译依赖见 [部署指南](docs/operations/deploy.md)。内核构建会从源码编译 V8。

在仓库根目录运行：

```sh
corepack enable
pnpm install --frozen-lockfile
pnpm build
cargo build --locked -p winwincode-cli --bin wwc
```

`pnpm build` 构建网页资源、Server、Worker 和内核辅助进程。最后一条命令构建 `wwc`。Rust 输出目录由 `CARGO_TARGET_DIR` 或 Cargo 配置决定。

## 文档

- [参与开发](CONTRIBUTING.md)：开发环境、测试和提交要求。
- [架构总览](docs/architecture/overview.md)：组件、通信和任务执行流程。
- [文档索引](docs/README.md)：契约、安全、上游维护和运维文档。
- [安全政策](SECURITY.md)：私密漏洞报告。

## 许可证

第一方代码使用 [Apache-2.0](LICENSE)。版权声明见 [NOTICE](NOTICE)，第三方许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
