# 部署 WinWinCode

本文说明当前源码的本机部署方式。已发布包的行为以对应版本的发布说明为准。

WinWinCode 由浏览器 Client、Server、Device Client、Worker 和内核辅助进程组成。Server 保存产品状态。Device Client 连接用户机器。Device Client 在本机启动 Worker。Provider 凭据也保存在 Device Client 所在机器。

## 选择部署来源

生产部署优先使用 GitHub Release 中同一平台的完整压缩包。不要混用不同版本或不同平台压缩包中的 `bin` 文件。

支持以下平台：

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-gnu`

下载所选平台的压缩包和外层 `SHA256SUMS`。外层清单包含四个平台的记录。先精确选择一个归档记录，并要求匹配数等于一。Linux 使用：

```sh
set -eu
version=0.2.0
target=x86_64-unknown-linux-gnu
archive="winwincode-${version}-${target}.tar.gz"
awk -v archive="$archive" '$2 == archive { count += 1; record = $0 } END { if (count != 1) exit 1; print record }' \
  SHA256SUMS > "${archive}.sha256"
sha256sum -c "${archive}.sha256"
```

macOS 使用：

```sh
set -eu
version=0.2.0
target=aarch64-apple-darwin
archive="winwincode-${version}-${target}.tar.gz"
awk -v archive="$archive" '$2 == archive { count += 1; record = $0 } END { if (count != 1) exit 1; print record }' \
  SHA256SUMS > "${archive}.sha256"
shasum -a 256 -c "${archive}.sha256"
```

校验通过后解压并进入包目录。包内 `SHA256SUMS` 是完整文件清单。不要从包内清单筛选记录。按同样的系统命令校验整个清单。

```sh
tar -xzf "$archive"
cd "winwincode-${version}-${target}"
```

Linux 使用：

```sh
sha256sum -c SHA256SUMS
```

macOS 使用：

```sh
shasum -a 256 -c SHA256SUMS
```

发布包包含这些主要目录和文件：

| 路径 | 用途 |
| --- | --- |
| `bin/winwincode-server` | Server |
| `bin/wwc` | 本地管理、Device Client、仓库接入和备份 |
| `bin/winwincode-worker` | Device Client 启动的 Worker |
| `bin/winwincode-kernel-helper` | Worker 使用的内核辅助进程 |
| `client/` | 浏览器静态资源 |
| `service/` | macOS `launchd` 或 Linux `systemd` 模板 |
| `verification/` | API 发布验证报告和来源封印副本 |
| `product-release-manifest.json`、`sbom.json` | 制品清单和依赖清单 |
| `legal/` | Apache-2.0 许可证、NOTICE 和第三方声明 |

`wwc`、Worker 和内核辅助进程保留在同一个 `bin` 目录。Device Client 按 `wwc` 的位置查找 Worker 和内核辅助进程。Worker 直接执行配置中的内核辅助进程路径。

macOS 二进制使用 ad-hoc 签名。当前发布包没有 Apple 公证。首次运行可能受到 Gatekeeper 或组织策略限制。

## 准备 Server

本机部署使用三个前台进程：Server、浏览器静态服务器和 Device Client。Server 默认监听 `127.0.0.1:8443`，使用 HTTP。浏览器界面默认连接 `http://127.0.0.1:8443`。首次运行会自动建立本地 Owner。

在源码仓库完成[构建](#从源码构建)后，打开第一个终端：

```sh
./target/debug/winwincode-server
```

Server 默认使用当前目录作为本地交付仓库，checkout revision 为 `HEAD`。需要交付候选改动时，该目录必须是有效 Git 仓库。可以用 `WWC_SERVER_REPOSITORY_ROOT` 指定其他仓库。

Server 数据默认保存在 `$HOME/.winwincode/server`。设置 `WINWINCODE_HOME` 后使用 `<WINWINCODE_HOME>/server`。`WWC_SERVER_DATA_DIRECTORY` 可以覆盖这个位置。

发布包使用 `./bin/winwincode-server`。以下命令中的 `./target/debug/wwc` 也相应改为 `./bin/wwc`。

### Server 环境变量

| 变量 | 默认值或用途 |
| --- | --- |
| `WWC_SERVER_BIND` | `127.0.0.1:8443`。 |
| `WWC_SERVER_PUBLIC_URL` | `http://127.0.0.1:8443`。Browser 和 Device 使用的对外 origin。 |
| `WWC_SERVER_ALLOWED_ORIGINS` | `http://127.0.0.1:8080,http://localhost:8080`。逗号分隔的浏览器 origin。 |
| `WWC_SERVER_PREVIEW_PUBLIC_URL` | 可选。预览使用的独立 origin。 |
| `WWC_SERVER_DATA_DIRECTORY` | 覆盖 Server 数据目录。 |
| `WINWINCODE_HOME` | 覆盖产品数据根目录。 |
| `WWC_SERVER_REPOSITORY_ROOT` | 当前工作目录。 |
| `WWC_SERVER_CHECKOUT_REVISION` | `HEAD`。可以指定精确 revision。 |
| `WWC_SERVER_ORGANIZATION_ID` | `org_00000000000000000000000000`。 |
| `WWC_SERVER_WORKSPACE_ID` | `wsp_00000000000000000000000000`。 |
| `WWC_SERVER_PROJECT_ID` | `prj_00000000000000000000000000`。 |
| `WWC_SERVER_REPOSITORY_ID` | `rep_00000000000000000000000000`。 |
| `WWC_SERVER_AUTH_MODE` | 本机回环 HTTP 默认 `local-open`。远程模式默认 `password`。 |
| `WWC_SERVER_TLS_CERTIFICATE` | Server 直接终止 TLS 时使用的 PEM 证书。 |
| `WWC_SERVER_TLS_PRIVATE_KEY` | 与证书配套的 PEM 私钥。 |
| `WWC_SERVER_TLS_PROXY` | 显式设为 `loopback` 时，使用受保护的回环 TLS 代理后端。 |
| `WWC_SERVER_BOOTSTRAP_PROOF` | `password` 模式的 Owner 初始化证明。 |
| `WWC_SERVER_BOOTSTRAP_WINDOW_SECONDS` | `600`。 |
| `WWC_SERVER_SESSION_TTL_SECONDS` | `28800`。 |
| `WWC_SERVER_EXECUTION_PROFILE` | `codex-chat`。 |
| `WWC_SERVER_MAX_RUNTIME_SECONDS` | `3600`。允许 `1..604800` 或 `unlimited`。 |
| `WWC_SERVER_MAX_ARTIFACT_BYTES` | `1073741824`。 |
| `WWC_SERVER_EXECUTION_LEASE_SECONDS` | `900`。 |
| `WWC_SERVER_WORKER_ID` | `wrk_00000000000000000000000001`。 |
| `WWC_SERVER_WORKER_POOL_ID` | `wpl_00000000000000000000000001`。 |
| `WWC_SERVER_WORKER_INSTANCE_ID` | 每次启动生成 `wki_` 标识。 |
| `WWC_SERVER_SCHEDULER_GENERATION` | 每次启动生成 `gen_` 标识。 |

GitHub 发布是可选功能。`GITHUB_REPOSITORY`、`GITHUB_CREDENTIAL_REFERENCE_ID`、`GITHUB_API_BASE_URL`、`SECRET_DIRECTORY`、`PUBLICATION_REQUESTERS`、`PUBLICATION_APPROVERS` 和 `PUBLICATION_APPROVAL_MAX_AGE_MILLIS` 全部非空时，Server 才装配发布适配器。配置不全时，Server 正常启动，发布功能不启用。发布凭据通过产品的 Credential Reference 流程保存。

### 认证与首次初始化

本机默认使用 `local-open`。Server 在空数据目录中自动建立 Owner，浏览器可以直接使用。

全新数据目录的 `password` 模式需要 `WWC_SERVER_BOOTSTRAP_PROOF`。Server 先监听 Owner 初始化入口。Owner 初始化完成后，Server 启动完整产品服务。初始化使用与正式服务相同的传输配置。操作见 [远程 Server 与 Device](#远程-server-与-device)。已有 local-open Owner 转换为 password 登录仍需要专门的账户运维流程。

`local-open` 将能访问 Server 的本机进程视为同一信任域。默认监听和 Compose 发布端口均限定在宿主回环地址。网络边界见[安全模型](../security-model.md)。

### 健康检查

```sh
curl --fail --show-error http://127.0.0.1:8443/health
```

就绪时返回 HTTP 200，JSON 中的 `status` 为 `ready`。内部应用不可用时返回 HTTP 503。该端点报告 Server 版本、协议 schema 和主要 API 路径。Device 和 Provider 的状态在浏览器设备页检查。

## 远程 Server 与 Device

Server 和 Device 可以位于两台机器。Device 主动连接 Server。Browser 使用 Server 的对外地址。Device 机器需要保留仓库、Provider 数据、Worker 和内核辅助进程。

### 配置 HTTPS

直接 TLS 示例使用 `https://server.example:8443`。部署时，把示例域名和 `/private/...` 路径替换为实际值。证书须覆盖该域名。证书、私钥和数据目录使用部署主机上的绝对路径。

```sh
export WWC_SERVER_BIND=0.0.0.0:8443
export WWC_SERVER_PUBLIC_URL=https://server.example:8443
export WWC_SERVER_ALLOWED_ORIGINS=https://app.example
export WWC_SERVER_AUTH_MODE=password
export WWC_SERVER_TLS_CERTIFICATE=/private/tls/server-chain.pem
export WWC_SERVER_TLS_PRIVATE_KEY=/private/tls/server-key.pem
export WWC_SERVER_DATA_DIRECTORY=/private/winwincode/server
export WWC_SERVER_BOOTSTRAP_PROOF='<initialization-proof>'
./target/debug/winwincode-server
```

代理终止 TLS 时，将应用监听地址设为回环 IP，例如 `127.0.0.1:8443`。对外 URL 仍为 HTTPS。设置 `WWC_SERVER_TLS_PROXY=loopback`，由受保护的本机代理转发到该监听地址。直接 TLS 和代理模式选择其一。远程模式拒绝 `local-open`。应用依据显式配置确认 TLS 边界，不依赖转发头。

浏览器静态资源也应通过允许的 HTTPS origin 提供。将 Client 的 `serverUrl` 配置为 `WWC_SERVER_PUBLIC_URL`。`WWC_SERVER_BIND` 只控制应用监听地址。

### 初始化 Owner

在另一个终端设置相同的 `WWC_SERVER_BOOTSTRAP_PROOF`，并在 bootstrap 窗口内调用 Owner 初始化入口。请求使用允许的 Origin 和该 Bearer proof。默认窗口为启动后的 600 秒。请求正文保存在权限受限的本机文件，例如 `/private/owner-bootstrap.json`：

```json
{"schemaVersion":"winwincode/v1","username":"owner","password":"<owner-password>"}
```

以下命令中的 CA 文件用于校验 Server 证书。使用公共受信任证书时，可省略 `--cacert`。

```sh
umask 077
chmod 600 /private/owner-bootstrap.json
curl --fail-with-body --show-error \
  --cacert /private/tls/root-ca.pem \
  --header 'Origin: https://app.example' \
  --header "Authorization: Bearer ${WWC_SERVER_BOOTSTRAP_PROOF}" \
  --header 'Content-Type: application/json' \
  --cookie-jar /private/owner.cookies \
  --data-binary @/private/owner-bootstrap.json \
  https://server.example:8443/api/v1/auth/session
```

首次初始化返回 HTTP 201，并建立 Owner 会话。Server 随后切换到完整服务。再次检查 HTTPS `/health`，等待 `status` 为 `ready`。初始化期间的业务请求可能返回 503。后续登录调用同一路径，提交用户名、密码和 Origin。普通登录不携带 bootstrap Bearer proof。Owner 建立后可从服务配置中移除 bootstrap proof，再重启 Server。

### 绑定远程 Device

Owner 使用已登录会话创建 10 分钟内有效的配对令牌：

```sh
curl --fail-with-body --show-error \
  --cacert /private/tls/root-ca.pem \
  --header 'Origin: https://app.example' \
  --header 'Content-Type: application/json' \
  --cookie /private/owner.cookies \
  --data '{"schemaVersion":"winwincode/v1"}' \
  https://server.example:8443/api/v1/devices/pairing
```

在 Device 机器上，把响应中的 `pairingToken` 放入 `WWC_DEVICE_PAIRING_TOKEN`，然后运行：

```sh
export WWC_DEVICE_PAIRING_TOKEN='<pairing-token>'
./target/debug/wwc device bind \
  --server-url https://server.example:8443 \
  --ca-file /private/tls/root-ca.pem \
  --data-dir /private/winwincode/device-client \
  --json
unset WWC_DEVICE_PAIRING_TOKEN
./target/debug/wwc device serve --data-dir /private/winwincode/device-client
```

`device bind` 成功时输出 `requestId`、`clientNodeId`、`publicClientId`、`ownerUserId` 和 `boundAt`。Device 在发出请求前保存密钥、Server URL 和稳定绑定请求。若成功响应丢失，重跑 `device bind --data-dir /private/winwincode/device-client --json`。Device 会复用原密钥、令牌和绑定意图，恢复原绑定。

已绑定 Device 的 `serve` 使用保存的 Server URL、身份和 CA。重启时无需重新配对。`--ca-file` 或 `WWC_DEVICE_CA_FILE` 可以显式配置 CA。Device 在起点解析证书，并验证主机名。Worker 的私有管道仍由 Device 创建和监管。

后续仓库和 Provider 配置沿用 [接入 Device Client](#接入-device-client) 的流程。使用自定义数据目录时，各管理命令都传相同的 `--data-dir`。

Owner 吊销设备时，向 `/api/v1/devices/<clientNodeId>/revoke` 发送带 Owner 会话的 POST 请求，正文为 `{"schemaVersion":"winwincode/v1"}`。在线 Device 确认吊销后停止 Worker。离线 Device 由本地租约截止时间限制继续执行。`device status --data-dir ... --json` 的 `device.service.authorizationFailure` 和 `device logs` 提供失败原因。两机故障验收步骤见 [测试指南](../testing.md#远程-device-验证)。

## 部署浏览器 Client

在源码仓库打开第二个终端：

```sh
node deploy/serve-client.mjs
```

访问 `http://127.0.0.1:8080`。脚本默认读取 `apps/client/dist/public`，并生成连接本机 Server 的 `/runtime-config.js`。它读取 `WWC_CLIENT_DIST`、`HOST`、`PORT` 和 `WWC_SERVER_URL`。使用其他浏览器地址时，把该精确 origin 加入 `WWC_SERVER_ALLOWED_ORIGINS`。

发布包中的 `client/` 可以交给普通静态服务器。例如：

```sh
python3 -m http.server 8080 --bind 127.0.0.1 --directory client
```

`client/runtime-config.js` 默认连接本机 Server。部署时可以覆盖该文件：

```js
// SPDX-License-Identifier: Apache-2.0
globalThis.__WINWINCODE_CLIENT_CONFIG__ = Object.freeze({
  serverUrl: 'http://127.0.0.1:8443',
})
```

先校验发布包，再修改部署副本。保留原始压缩包和校验文件。

## 接入 Device Client

在宿主机器打开第三个终端：

```sh
./target/debug/wwc device serve
```

Device 默认连接 `http://127.0.0.1:8443`。数据目录默认是 `$HOME/.winwincode/device-client`。设置 `WINWINCODE_HOME` 后使用 `<WINWINCODE_HOME>/device-client`。`WWC_DEVICE_DATA_DIRECTORY` 或 `--data-dir` 可以覆盖数据目录。首次本机启动可用 `WWC_DEVICE_SERVER_URL` 或 `--server-url` 指定 Server 地址。已绑定远程 Device 优先复用保存的 Server URL。

Device Client 以前台进程运行。它启动并监管 Worker。Worker 通过 stdin/stdout JSON-lines 与父 Device 通信，Device 将交换请求转发给 Server。Worker 的启动参数由 Device 写入 `managed-session.json`。

| 内部参数 | 来源与用途 |
| --- | --- |
| `workerId`、`workerInstanceId`、`workerSessionId` | 启动会话时生成。用于关联执行消息、进程和任务，由程序管理。 |
| `sourceDirectory` | Device 已登记仓库的本机路径。任务从这里读取源码。 |
| `dataDirectory` | `<device-data>/worker-sessions/<workerSessionId>/data`。保存该 Worker 的执行状态。 |
| `executionWorkspaceDirectory` | Device 为会话准备的工作区。任务改动在这里执行。 |
| `providerDirectory` | `<device-data>/providers`。实际模型调用时读取。 |
| `helperExecutable` | Device 解析出的辅助进程绝对路径。执行任务时直接启动。 |

| 配置 | 默认值和覆盖方式 |
| --- | --- |
| Worker 程序 | `wwc` 同目录的 `winwincode-worker`。 |
| 内核辅助进程 | `wwc` 同目录的 `winwincode-kernel-helper`。可以用 `WWC_WORKER_HELPER_EXECUTABLE` 覆盖。 |

显式程序路径相对于 Device 启动目录解析。Device 在切换 Worker 工作目录前将路径转换为绝对路径。

在另一个终端检查状态并生成连接码：

```sh
./target/debug/wwc device status
./target/debug/wwc device refresh-code
```

在浏览器设备页提交连接码。随后在项目目录注册当前仓库：

```sh
./target/debug/wwc repo add
./target/debug/wwc repo list
```

`repo add` 默认使用当前目录。也可以显式传入其他仓库路径。

在浏览器设置页配置该 Device 的 Provider，然后提交任务。Provider 配置在浏览器中加密，由 Device 解密并保存在 `<device-data>/providers/providers.sqlite3`。Worker 从该目录读取配置。

其他管理命令共用相同默认数据目录：

```sh
./target/debug/wwc device logs
./target/debug/wwc device lock
./target/debug/wwc device unlock
```

`wwc init` 和 `wwc repo attach` 处理本地仓库基线。`wwc repo add --init` 可以初始化尚未成为 Git 仓库的目录。Snapshot 操作要求显式确认。

发布包中的 `service/` 模板支持 `launchd` 或用户级 `systemd`。将 `__WWC_BINARY__` 替换为 `wwc` 的绝对路径。模板使用相同默认值。Linux 可在 `$HOME/.config/winwincode/device-client.env` 中覆盖环境变量。

## 从源码构建

| 工具 | 要求 |
| --- | --- |
| Node.js | `.node-version` 为 24.19.0；运行检查接受 24.x。 |
| pnpm | `package.json` 固定为 11.7.0。通过 Corepack 调用。 |
| Rust | `rust-toolchain.toml` 固定为 1.95.0，并包含 `rust-src`、`clippy` 和 `rustfmt`。 |
| Git | 仓库操作和来源校验使用。 |

macOS 需要 Apple 编译工具链和 libclang。发布流水线安装 Homebrew `llvm@19`，并设置 `LIBCLANG_PATH` 和 `CLANG_PATH`。

Linux 支持 Ubuntu 24.04 和 Debian 12 的依赖组合。`scripts/install-code-mode-linux-build-deps.sh` 安装 LLVM 23、Clang 23、libclang、compiler-rt、lld、binutils、`pkg-config`、`libcap-dev`、`libglib2.0-dev` 和 `bubblewrap`。它还准备 Rust V8 构建需要的 compiler-rt 路径。

`.cargo/config.toml` 强制 `V8_FROM_SOURCE=1`。首次完整构建会下载并编译 V8。构建机需要外网、足够的磁盘空间和较长的编译时间。

```sh
corepack enable
corepack pnpm install --frozen-lockfile
corepack pnpm build
```

`pnpm build` 产出浏览器 Client、`wwc`、Server、Worker 和内核辅助进程。Rust 程序默认位于 `target/debug/`。设置 `CARGO_TARGET_DIR` 后使用该目录。`pnpm start` 运行 Server 的开发入口。

## Docker Compose

Compose 在容器中运行 Server 和 Web。Device Client 在宿主机器上运行，以访问宿主仓库和 Provider 数据。Server 使用 host networking 监听宿主回环地址。

该配置适用于 Linux Docker Engine。Docker Desktop 需要 4.34 或更高版本，并在 Settings → Resources → Network 启用 Enable host networking，再应用并重启。该功能与 Enhanced Container Isolation 不兼容。平台要求见 [Docker host networking 文档](https://docs.docker.com/engine/network/drivers/host/)。未启用该功能时，使用 [本机二进制部署](#准备-server)。

先在宿主完成源码构建，然后执行：

```sh
docker compose up --build -d
./target/debug/wwc device serve
```

访问 `http://127.0.0.1:8080`。按照[接入 Device Client](#接入-device-client)生成连接码、连接设备、注册宿主仓库和配置 Provider。

Server 容器通过 host networking 监听 `127.0.0.1:${WWC_SERVER_PORT:-8443}`。浏览器和宿主 Device 都使用该宿主 HTTP 地址。Web 端口仍发布到宿主回环地址。Web 容器只提供静态资源，浏览器自行连接 Server。Server 健康检查使用相同的回环地址和端口。

`backend-data` 卷保存 Server 数据。Compose 把 `${WWC_SERVER_REPOSITORY_ROOT:-.}` 绑定到容器 `/workspace/repository`。默认使用当前仓库目录。该目录需要包含可在容器内读取的 Git 元数据。Git worktree 的 `.git` 文件可能指向宿主上的外部目录；这种情况下，设置 `WWC_SERVER_REPOSITORY_ROOT` 指向普通 clone。

`deploy/compose.env.example` 列出版本、Web 端口、Server 端口和宿主仓库路径。调整 Server 端口后，宿主 Device 也要使用对应的 `--server-url`。

## 数据目录与 schema

Server 的主要数据包括：

- 数据根目录中的 `control-plane.sqlite3` 和其他根级 SQLite 库。
- `auth-sessions/auth-sessions.sqlite3`。
- `event-hub/server-event-hub.sqlite3`。
- `artifact-catalog/`、`artifact-catalog-collaboration/` 和 `artifacts/`。
- 发布凭据目录 `SECRET_DIRECTORY`。它不应放入普通日志或公开备份。

Device Client 的主要数据包括：

- `device-client.sqlite3`。
- `providers/providers.sqlite3`。
- `worker-sessions/`、`execution-workspaces/`、日志和临时运行状态。

`control-plane.sqlite3` 当前 schema 为 `8`，`device-client.sqlite3` 当前 schema 为 `11`。两者分别由 `SqliteStorage` 与 `DeviceStore` 的 `initialize_schema` 检查 `user_version`，在同一事务内初始化和校验。版本不匹配时拒绝打开。这个行为不能推广到其他产品数据库。

Gate 决策和运行追踪还会检查持久化 JSON 的字段结构。SQLite 版本匹配不代表这些记录可读。恢复备份时须使用与记录结构匹配的程序。

Worker 的 Codex 数据目录仍有迁移和回填路径。重开旧目录可能修改 schema、迁移标记或持久记录。打开前先保留原目录和匹配程序的备份。实际实现与已批准的开发目标见 [当前数据库 schema](../invariants.md#当前数据库-schema)。已发布用户数据的升级政策尚未定义，发布说明必须逐次说明兼容范围。

升级前先备份。备份只提供与原 schema 匹配的恢复点。`wwc backup` 不转换 schema，恢复操作也不执行 schema 升级。不要把备份流程当作升级流程。降级时恢复旧程序及与它匹配的数据备份。不要让不同版本的 Server 或 Device Client 共享同一数据目录。

## 备份、校验和恢复

`wwc backup` 支持 `server` 和 `device` 两种 store。它使用 SQLite `VACUUM INTO` 生成一致性快照，不会热拷贝 SQLite 文件。

当前备份范围有限。Server backup 只发现 `WWC_SERVER_DATA_DIRECTORY` 根目录下的 `*.sqlite3`。它不包含嵌套的 auth session、event hub、artifact catalog、artifact object 或 `SECRET_DIRECTORY`。Device backup 只包含 `device-client.sqlite3`。它不包含 `providers/providers.sqlite3`、Provider 凭据或 Worker 工作目录。

因此，`wwc backup` 当前不是完整灾难恢复包。运行下列命令前，先为未覆盖数据制定单独的停机备份方案。凭据必须按组织的秘密管理政策单独处理。不要热拷贝未覆盖的 SQLite 文件。

Server 快照：

```sh
./bin/wwc backup snapshot \
  --store server \
  --data-dir /srv/winwincode/server-data \
  --output /srv/backups/winwincode-server-2026-10-14
```

Device Client 快照：

```sh
./bin/wwc backup snapshot \
  --store device \
  --data-dir "$WWC_DEVICE_DATA_DIRECTORY" \
  --output /srv/backups/winwincode-device-2026-10-14
```

输出目录必须不存在或为空。它不能位于数据目录内。备份包含 `manifest.json`、SQLite 快照、大小、SHA-256、schema 版本和完整性结果。

校验备份：

```sh
./bin/wwc backup verify \
  --from /srv/backups/winwincode-server-2026-10-14
```

恢复前停止对应的 Server 或 Device Client。先校验，再恢复：

```sh
./bin/wwc backup verify --from /srv/backups/winwincode-server-2026-10-14
./bin/wwc backup restore \
  --store server \
  --data-dir /srv/winwincode/server-data \
  --from /srv/backups/winwincode-server-2026-10-14
```

Device 恢复使用相同参数，并把 `--store` 改为 `device`。Device 快照保存设备身份和运行状态，可以恢复到空目录。恢复前停止原 Device，避免两个进程使用同一设备身份。

诊断默认只读：

```sh
./bin/wwc backup repair \
  --store server \
  --data-dir /srv/winwincode/server-data
```

`--apply` 只执行白名单动作，例如 SQLite checkpoint 和清理遗留临时文件。每次诊断或修复都会追加 `backup-repair-log.jsonl`。

所有 backup 子命令都支持 `--json`。Server store 未显式传 `--data-dir` 时会读取 `WWC_SERVER_DATA_DIRECTORY`。Device store 始终要求 `--data-dir`。

## 故障定位

- Server 启动即退出：检查绑定端口、数据目录权限和数据库 schema。
- `/health` 返回 503：查看 Server 标准错误输出。
- 浏览器连接失败：检查 `runtime-config.js`、Server 地址和 `WWC_SERVER_ALLOWED_ORIGINS`。
- Device 离线：检查 `wwc device status`、Server 地址和 Device 日志。
- 任务提示没有 Provider：在浏览器设置页为该 Device 配置 Provider。
- Worker 启动失败：检查 `wwc device logs`，以及 Worker 和内核辅助进程的实际路径与执行权限。
- 数据库提示 unsupported schema version：停止进程，恢复匹配版本的程序和备份。

## 相关文档

- [项目说明](../../README.md)
- [安全政策](../../SECURITY.md)
- [测试指南](../testing.md)
- [安全模型](../security-model.md)
- [上游维护](../upstream.md)
- [发布指南](release.md)
