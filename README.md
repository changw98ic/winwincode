# WinWinCode

WinWinCode 是可自托管的 AI 编程工具。你在浏览器中提交任务，连接的设备运行内嵌 Codex 内核，执行代码修改和检查。

- **Chat**：连续对话，提交编程任务，查看模型和工具的执行记录。
- **StrongFlow**：将需求、方案、执行、评审和验证组织成交付流程。
- **候选改动**：查看任务产生的代码差异、验证证据和交付状态。

## 构建与使用

在源码仓库执行：

```sh
corepack pnpm install --frozen-lockfile
corepack pnpm build
```

构建产出 `wwc`、Server、Worker、内核辅助进程和浏览器界面。环境要求见[从源码构建](docs/operations/deploy.md#从源码构建)。

在三个终端分别启动：

```sh
./target/debug/winwincode-server
node deploy/serve-client.mjs
./target/debug/wwc device serve
```

访问 `http://127.0.0.1:8080`。生成设备连接码并注册仓库：

```sh
./target/debug/wwc device refresh-code
./target/debug/wwc repo add /absolute/path/to/repository
```

在浏览器设备页提交连接码，在设置页配置模型，然后提交任务。Server 默认监听 `http://127.0.0.1:8443`。产品数据默认保存在 `$HOME/.winwincode`。

发布包和 Docker Compose 的使用步骤见[部署指南](docs/operations/deploy.md)。发布包支持 macOS 和 GNU Linux 的 ARM64、x64，下载入口是 [GitHub Releases](https://github.com/changw98ic/winwincode/releases)。

## 文档

- [参与开发](CONTRIBUTING.md)：开发环境、测试和提交要求。
- [架构总览](docs/architecture/overview.md)：组件、通信和任务执行流程。
- [文档索引](docs/README.md)：契约、安全、上游维护和运维文档。
- [安全政策](SECURITY.md)：私密漏洞报告。

## 许可证

第一方代码使用 [Apache-2.0](LICENSE)。版权声明见 [NOTICE](NOTICE)，第三方许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
