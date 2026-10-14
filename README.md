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
3. 在浏览器中连接设备并配置模型。

这些步骤可以建立 Device 连接。managed Worker 还需要部署者提供与 Server 一致的 `WWC_WORKER_ACTION_SIGNING_KEY_HEX`。配置齐全前，不应把 Device 已连接视为 Chat 或 StrongFlow 任务已经可以执行。所需变量见[接入 Device Client](docs/operations/deploy.md#接入-device-client)。

0.2.0 的新库 password 初始化流程尚未接通。首次部署请使用指南中的本机回环 `local-open` 配置。

## 从源码构建

环境、工具链和完整构建步骤见 [部署指南](docs/operations/deploy.md#从源码构建)。

## 文档

- [参与开发](CONTRIBUTING.md)：开发环境、测试和提交要求。
- [架构总览](docs/architecture/overview.md)：组件、通信和任务执行流程。
- [文档索引](docs/README.md)：契约、安全、上游维护和运维文档。
- [安全政策](SECURITY.md)：私密漏洞报告。

## 许可证

第一方代码使用 [Apache-2.0](LICENSE)。版权声明见 [NOTICE](NOTICE)，第三方许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
