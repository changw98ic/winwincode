# WinWinCode

可自托管的 AI 编程工具：在浏览器里提交编程任务，由你自己机器上的设备用内嵌 Codex 内核执行。

> [!WARNING]
> **当前状态：Alpha（0.2.0），还不能用来跑任务。**
>
> - **发布包不能执行任务。** Server、浏览器界面和 Device Client 能启动并互连，但发布包缺少 Worker 的生产装配（`WWC_WORKER_EXECUTION_ENVELOPE_DIGEST` 的生成与下发）。Chat 和 StrongFlow 任务目前不会真正执行。
> - **password 认证不可用。** 新数据目录只能用 `local-open`（无登录）模式启动，因此只适合本机回环、单用户环境。不要暴露到网络。
> - **Docker Compose 不可用。** `compose.yaml` 与 0.2.0 不匹配，不能直接 `docker compose up`。
>
> 细节见[部署指南](docs/operations/deploy.md)。

## 能做什么 / 不能做什么

| 能做（0.2.0） | 还不能做 |
| --- | --- |
| 下载并校验四个平台的发布包 | 在发布包上执行 Chat / StrongFlow 任务 |
| 在本机以 `local-open` 启动 Server，检查 `/health` | 用 password 模式初始化新部署 |
| 部署浏览器 Client，接入 Device Client、绑定仓库、配置模型 | 用 Docker Compose 部署 |
| 从源码构建并运行测试 | 公网或多用户部署 |

## 快速开始（本机，local-open）

支持平台：`aarch64-apple-darwin`、`x86_64-apple-darwin`、`aarch64-unknown-linux-gnu`、`x86_64-unknown-linux-gnu`。

需要：`curl`、`git`、`openssl`，以及一张包含 `localhost` 的 TLS 证书和私钥（Server 只接受 HTTPS）。本机试用可以生成自签名证书：

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 30 \
  -keyout server.key -out server.crt \
  -subj /CN=localhost -addext subjectAltName=DNS:localhost
```

### 1. 下载并校验（Linux x64 示例）

```sh
set -eu
version=0.2.0
target=x86_64-unknown-linux-gnu
archive="winwincode-${version}-${target}.tar.gz"
base="https://github.com/changw98ic/winwincode/releases/download/v${version}"
curl -fLO "${base}/${archive}"
curl -fLO "${base}/SHA256SUMS"
grep " ${archive}\$" SHA256SUMS | sha256sum -c -
tar -xzf "$archive"
cd "winwincode-${version}-${target}"
sha256sum -c SHA256SUMS
```

macOS 把 `target` 换成对应值，并用 `shasum -a 256 -c` 替代 `sha256sum -c`。

### 2. 准备仓库和数据目录

```sh
install -d -m 700 "$HOME/winwincode/server-data" "$HOME/winwincode/publication-secrets"
git -C "$HOME/winwincode/repository" rev-parse 'HEAD^{tree}'
```

`$HOME/winwincode/repository` 须是已有的 Git 仓库。记下输出的 tree id。

### 3. 启动 Server

以下变量全部是当前 Server 入口（`crates/winwincode-server/src/main.rs`）的必填项。即使不用 GitHub 发布功能，`GITHUB_*` 和 `PUBLICATION_*` 也必须设置。

```sh
export WWC_SERVER_BIND=127.0.0.1:8443
export WWC_SERVER_PUBLIC_URL=https://localhost:8443
export WWC_SERVER_ALLOWED_ORIGINS=http://localhost:8080
export WWC_SERVER_AUTH_MODE=local-open
export WWC_SERVER_DATA_DIRECTORY="$HOME/winwincode/server-data"
export WWC_SERVER_REPOSITORY_ROOT="$HOME/winwincode/repository"
export WWC_SERVER_CHECKOUT_REVISION='git-tree:<上一步输出的 tree id>'
export WWC_SERVER_BOOTSTRAP_PROOF="$(openssl rand -hex 32)"
export WWC_SERVER_ACTION_SIGNING_KEY_HEX="$(openssl rand -hex 32)"
export WWC_SERVER_TLS_CERTIFICATE=/path/to/server.crt
export WWC_SERVER_TLS_PRIVATE_KEY=/path/to/server.key
export WWC_SERVER_ORGANIZATION_ID=org_00000000000000000000000000
export WWC_SERVER_WORKSPACE_ID=wsp_00000000000000000000000000
export WWC_SERVER_PROJECT_ID=prj_00000000000000000000000000
export WWC_SERVER_REPOSITORY_ID=rep_00000000000000000000000000

export GITHUB_REPOSITORY=owner/repository
export GITHUB_CREDENTIAL_REFERENCE_ID=crd_00000000000000000000000000
export GITHUB_API_BASE_URL=https://api.github.com
export SECRET_DIRECTORY="$HOME/winwincode/publication-secrets"
export PUBLICATION_REQUESTERS=usr_00000000000000000000000000
export PUBLICATION_APPROVERS=usr_00000000000000000000000000
export PUBLICATION_APPROVAL_MAX_AGE_MILLIS=3600000

./bin/winwincode-server
```

另开终端检查：

```sh
curl --fail --cacert /path/to/server.crt https://localhost:8443/health
```

返回 `"status":"ready"` 即启动成功。浏览器 Client 和 Device Client 的接入见[部署指南](docs/operations/deploy.md#部署浏览器-client)。完整变量表见 [Server 环境变量](docs/operations/deploy.md#server-环境变量)。

## 从源码构建和测试

| 工具 | 版本（来源） |
| --- | --- |
| Node.js | 24.x，推荐 24.19.0（`.node-version`、`package.json` engines） |
| pnpm | 11.7.0，通过 Corepack（`package.json` packageManager） |
| Rust | 1.95.0，含 clippy、rustfmt、rust-src（`rust-toolchain.toml`） |

Linux 额外依赖可用 `scripts/install-code-mode-linux-build-deps.sh` 安装。首次构建会从源码编译 V8，需要外网、大量磁盘和较长时间。

```sh
corepack enable
corepack pnpm install --frozen-lockfile
corepack pnpm build                                  # Client、Server、Worker、内核辅助进程
cargo build --locked -p winwincode-cli --bin wwc     # 管理 CLI，pnpm build 不包含

corepack pnpm test                                   # TypeScript + Rust 测试
corepack pnpm lint                                   # 源码规则、类型检查、clippy
corepack pnpm verify:docs                            # 文档格式与链接检查
corepack pnpm verify                                 # CI 级完整门禁（很慢）
```

按改动范围选择检查见[测试指南](docs/testing.md#定向验证)。

## 五个核心概念

| 术语 | 含义 |
| --- | --- |
| Server | 控制面。保存状态，提供浏览器 API，调度任务。 |
| Client | 浏览器中的前端（静态站点）。 |
| Device Client | 跑在你的执行机器上的守护进程（`wwc device serve`），负责连接 Server、绑定本地仓库、保存模型凭据。 |
| Worker | 由 Device Client 启动、实际运行 Codex 内核执行任务的进程。 |
| StrongFlow | 按“需求 → 方案 → 执行 → 评审 → 验证”组织任务的固定交付流程。 |

其余术语见[术语表](docs/glossary.md)，架构见[架构总览](docs/architecture/overview.md)。

## 参与开发

先读 [CONTRIBUTING.md](CONTRIBUTING.md)。全部文档入口见[文档索引](docs/README.md)。安全问题请按 [SECURITY.md](SECURITY.md) 私密报告。

## 许可证

第一方代码使用 [Apache-2.0](LICENSE)。版权声明见 [NOTICE](NOTICE)，第三方许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
