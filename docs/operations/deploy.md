# 部署 WinWinCode

本文说明 WinWinCode 0.2.0 的实际部署边界。目标读者是自托管运维人员。

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
| `bin/winwincode-kernel-helper.release.json` | 内核辅助进程的签名清单 |
| `client/` | 浏览器静态资源 |
| `service/` | macOS `launchd` 或 Linux `systemd` 模板 |
| `verification/` | API 发布验证报告和来源封印副本 |
| `product-release-manifest.json`、`sbom.json` | 制品清单和依赖清单 |
| `legal/` | Apache-2.0 许可证、NOTICE 和第三方声明 |

`wwc`、Worker、内核辅助进程和签名清单必须保留在同一个 `bin` 目录。Device Client 按 `wwc` 的位置查找同目录的 `winwincode-worker`。Worker 会校验内核辅助进程及其签名清单。

macOS 二进制使用 ad-hoc 签名。当前发布包没有 Apple 公证。首次运行可能受到 Gatekeeper 或组织策略限制。

## 准备 Server

Server 需要一个固定数据目录和一个本地 Git 仓库。Server 配置中的 checkout revision 必须是精确的 Git tree 标识。可以这样生成：

```sh
git -C /srv/winwincode/repository rev-parse 'HEAD^{tree}'
```

在输出前添加 `git-tree:`。例如 `git-tree:0123...`。

当前生产 Server 入口总会装配远程 Worker 交换。该交换要求 Server 自己启用 TLS 监听器。生产入口缺少证书或私钥时会在监听前失败。`WWC_SERVER_PUBLIC_URL` 因此必须使用 `https://`。反向代理可以位于 Server 前方，但代理到 Server 的连接仍须使用 Server 的 HTTPS 监听器。当前生产入口不支持由反向代理终止 TLS 后再连接纯 HTTP Server。

下面的示例适用于一台机器上的首次本地部署。它把 Server 绑定到回环地址，并使用 `local-open` 认证。它仍然需要一组发布配置，因为当前 Server 启动时总会装配 GitHub 发布适配器。

运行前先准备有效 Git 仓库，以及包含 `localhost` 名称的 TLS 证书和私钥。将所有路径和 `<...>` 占位值替换为实际配置。操作者需要对示例数据目录有创建权限。

```sh
install -d -m 700 /srv/winwincode/server-data
install -d -m 700 /srv/winwincode/publication-secrets

export WWC_SERVER_BIND=127.0.0.1:8443
export WWC_SERVER_PUBLIC_URL=https://localhost:8443
export WWC_SERVER_ALLOWED_ORIGINS=http://localhost:8080
export WWC_SERVER_DATA_DIRECTORY=/srv/winwincode/server-data
export WWC_SERVER_REPOSITORY_ROOT=/srv/winwincode/repository
export WWC_SERVER_CHECKOUT_REVISION='git-tree:<exact-tree-object-id>'
export WWC_SERVER_BOOTSTRAP_PROOF='<random-one-time-proof>'
export WWC_SERVER_AUTH_MODE=local-open
export WWC_SERVER_ORGANIZATION_ID=org_00000000000000000000000000
export WWC_SERVER_WORKSPACE_ID=wsp_00000000000000000000000000
export WWC_SERVER_PROJECT_ID=prj_00000000000000000000000000
export WWC_SERVER_REPOSITORY_ID=rep_00000000000000000000000000
export WWC_SERVER_ACTION_SIGNING_KEY_HEX='<64-lowercase-hex-characters>'
export WWC_SERVER_TLS_CERTIFICATE=/etc/winwincode/server.crt
export WWC_SERVER_TLS_PRIVATE_KEY=/etc/winwincode/server.key

export GITHUB_REPOSITORY=owner/repository
export GITHUB_CREDENTIAL_REFERENCE_ID=crd_00000000000000000000000000
export GITHUB_API_BASE_URL=https://api.github.com
export SECRET_DIRECTORY=/srv/winwincode/publication-secrets
export PUBLICATION_REQUESTERS=usr_00000000000000000000000000
export PUBLICATION_APPROVERS=usr_00000000000000000000000000
export PUBLICATION_APPROVAL_MAX_AGE_MILLIS=3600000

./bin/winwincode-server
```

可以分别用 `openssl rand -hex 32` 生成 action signing key 和 bootstrap proof。不要跨用途复用相同随机值。不要把这些值写入仓库或进程日志。

完成这些前置配置后，使用示例启动 Server，并按下文检查 `/health`。GitHub 发布操作还需要在产品的 credential reference 流程中写入匹配的凭据。仅创建 `SECRET_DIRECTORY` 或设置 `GITHUB_CREDENTIAL_REFERENCE_ID` 不会创建凭据。

### Server 环境变量

下表列出 `crates/winwincode-server/src/main.rs` 当前读取的运行配置。

| 变量 | 要求和默认值 |
| --- | --- |
| `WWC_SERVER_BIND` | 必填。Socket 地址，例如 `127.0.0.1:8443`。 |
| `WWC_SERVER_PUBLIC_URL` | 必填。只允许 `https://` origin。不能包含路径、查询、凭据或 fragment。 |
| `WWC_SERVER_ALLOWED_ORIGINS` | 必填。逗号分隔的浏览器 origin。至少一个。 |
| `WWC_SERVER_PREVIEW_PUBLIC_URL` | 可选。必须是独立的 `https://` origin。 |
| `WWC_SERVER_DATA_DIRECTORY` | 必填。Server 数据根目录。 |
| `WWC_SERVER_REPOSITORY_ROOT` | 必填。Server 的本地交付仓库。 |
| `WWC_SERVER_CHECKOUT_REVISION` | 必填。执行配置使用的精确 checkout revision。 |
| `WWC_SERVER_ORGANIZATION_ID` | 必填。规范 `org_` 标识。 |
| `WWC_SERVER_WORKSPACE_ID` | 必填。规范 `wsp_` 标识。 |
| `WWC_SERVER_PROJECT_ID` | 必填。规范 `prj_` 标识。 |
| `WWC_SERVER_REPOSITORY_ID` | 必填。规范 `rep_` 标识。 |
| `WWC_SERVER_BOOTSTRAP_PROOF` | 必填。即使使用 `local-open`，当前入口也要求该值。 |
| `WWC_SERVER_ACTION_SIGNING_KEY_HEX` | 必填。32 字节密钥的小写十六进制表示。 |
| `WWC_SERVER_AUTH_MODE` | 可选。值为 `local-open` 或 `password`。未设置时，回环监听默认 `local-open`，其他监听地址默认 `password`。 |
| `WWC_SERVER_BOOTSTRAP_WINDOW_SECONDS` | 默认 `600`。 |
| `WWC_SERVER_SESSION_TTL_SECONDS` | 默认 `28800`。 |
| `WWC_SERVER_EXECUTION_PROFILE` | 默认 `codex-chat`。 |
| `WWC_SERVER_MAX_RUNTIME_SECONDS` | 默认 `3600`。允许 `1..604800` 或 `unlimited`。 |
| `WWC_SERVER_MAX_ARTIFACT_BYTES` | 默认 `1073741824`。 |
| `WWC_SERVER_EXECUTION_LEASE_SECONDS` | 默认 `900`。必须是有效的正整数秒数。 |
| `WWC_SERVER_WORKER_ID` | 默认 `wrk_00000000000000000000000001`。 |
| `WWC_SERVER_WORKER_POOL_ID` | 默认 `wpl_00000000000000000000000001`。 |
| `WWC_SERVER_WORKER_INSTANCE_ID` | 可选。未设置时每次启动生成 `wki_` 标识。 |
| `WWC_SERVER_SCHEDULER_GENERATION` | 可选。未设置时每次启动生成 `gen_` 标识。 |
| `WWC_SERVER_TLS_CERTIFICATE` | 当前生产入口必填。PEM 证书路径。必须与私钥同时设置。 |
| `WWC_SERVER_TLS_PRIVATE_KEY` | 当前生产入口必填。PEM 私钥路径。必须与证书同时设置。 |
| `WWC_SERVER_CREDENTIAL_HASH_CONCURRENCY` | 可选。正整数。无效或缺失时使用可用 CPU 并行数。 |
| `GITHUB_REPOSITORY` | 必填。格式为 `owner/repository`。 |
| `GITHUB_CREDENTIAL_REFERENCE_ID` | 必填。规范 `crd_` 标识。 |
| `GITHUB_API_BASE_URL` | 必填。GitHub API 地址。 |
| `SECRET_DIRECTORY` | 必填。发布凭据的本地安全存储根目录。 |
| `PUBLICATION_REQUESTERS` | 必填。逗号分隔。至少一个身份。 |
| `PUBLICATION_APPROVERS` | 必填。逗号分隔。至少一个用户标识。 |
| `PUBLICATION_APPROVAL_MAX_AGE_MILLIS` | 必填。审批最大时效，单位为毫秒。 |

当前 Server 不读取 `WWC_SERVER_WORKER_MODE`、`WWC_SERVER_REMOTE_WORKER_CREDENTIAL_FILE`、`WWC_SERVER_REMOTE_WORKER_EXPIRES_AT` 或 `WWC_SERVER_SOURCE_ROOT`。设置这些变量不会改变 Server 的装配方式。Server 当前总是提供远程执行交换和 Device Client 交换端点。

### 认证与首次初始化

首次使用空数据目录时，只有 `local-open` 能完成启动。`SqliteAuthSessionManager::ensure_local_owner` 会自动创建本地 Owner。`password` 模式不会自动创建 Owner，而 `open_production_application` 要求 Owner 在开始监听前已经存在。因此，新数据目录不能通过启动 password Server 后再访问网页完成初始化。

当前也没有从这条 `local-open` 初始化路径切换到 password 模式的完整运维步骤。自动创建的 Owner 密码是随机值，程序不会显示它。`change_owner_password` 又要求提交当前密码。当前 `wwc help` 没有用户或 Owner 管理命令。因此，不要把“先用 `local-open` 创建 Owner，再切换到 `password`”写入自动化。新库的 password 初始化仍是当前产品缺口。

不要把“空数据目录、非回环监听、默认认证模式”当成可用的首次部署。需要公网或共享网络部署时，必须先补齐受支持的 Owner 初始化流程，或使用已经正确初始化且与当前 schema 匹配的数据备份。

`local-open` 不要求浏览器登录。它只适合受保护的本地或单用户环境。不要把 `local-open` Server 直接暴露到不受信任网络。认证和网络边界见[安全模型](../security-model.md)。

### 健康检查

Server 提供 `GET /health`。就绪时返回 HTTP 200，JSON 中的 `status` 为 `ready`。内部应用不可用时返回 HTTP 503。

```sh
curl --fail --show-error --cacert /etc/winwincode/server.crt \
  https://localhost:8443/health
```

该端点报告 Server 版本、协议 schema 和主要 API 路径。它不证明 Device Client 已连接，也不证明 Provider 已配置。

## 部署浏览器 Client

`client/` 是静态站点。部署时必须提供 `runtime-config.js`，并把 `serverUrl` 设置为 Server origin。

```js
// SPDX-License-Identifier: Apache-2.0
globalThis.__WINWINCODE_CLIENT_CONFIG__ = Object.freeze({
  serverUrl: 'https://localhost:8443',
})
```

可以在校验并解压后复制一份 `client/` 作为部署目录，再修改部署副本。保留原始压缩包和校验文件作为未修改证据。

静态服务器必须正确提供 `index.html`、`runtime-config.js` 和 `assets/`。Server 的 `WWC_SERVER_ALLOWED_ORIGINS` 必须包含浏览器站点的精确 origin。

源码树中的 `deploy/serve-client.mjs` 会动态生成 `/runtime-config.js`。它需要 Node.js 24，并读取 `WWC_CLIENT_DIST`、`HOST`、`PORT` 和 `WWC_SERVER_URL`。正式发布包不包含这个脚本。发布包部署需要自备静态服务器或反向代理。

## 接入 Device Client

以下步骤只完成 Device Client 服务接入、设备连接、仓库注册和 Provider 配置。它们不能单独证明任务可执行。managed Worker 还需要下表中的运行配置。不要使用测试 fixture 的固定密钥补齐生产配置。

Device supervisor 启动 Worker 时会继承 Device Client 进程的环境。它只额外设置 `WWC_MANAGED_START_GATE=1`，并写入包含会话身份、仓库、目录、Server origin 和 Worker 凭据路径的 managed config。该 config 不包含下表中的 Worker 运行配置。

| 变量 | 当前要求和来源 |
| --- | --- |
| `WWC_WORKER_TLS_ROOT_DER_FILE` | `run_worker` 必填。它应指向 Worker 连接 Server 时信任的 DER 根证书。未设置时，Device supervisor 使用 `WWC_DEVICE_TLS_ROOT_DER_FILE`。需要不同根证书时显式设置该变量。 |
| `WWC_WORKER_ACTION_SIGNING_KEY_HEX` | `run_worker` 和 `production_codex` 必填。它从 Device Client 的环境继承，由部署者通过受保护的服务配置或部署环境提供。它必须与 Server 的 `WWC_SERVER_ACTION_SIGNING_KEY_HEX` 使用同一 32 字节动作签名密钥。不要把密钥写入仓库或日志。 |
| `WWC_WORKER_HELPER_RELEASE_MANIFEST` | `production_codex` 必填。未设置时，Device supervisor 使用 Worker 同目录的 `winwincode-kernel-helper.release.json`。Worker 会验证清单签名及绑定信息。 |
| `WWC_WORKER_HELPER_EXECUTABLE` | `production_codex` 必填。未设置时，Device supervisor 使用 Worker 同目录的 `winwincode-kernel-helper`。显式设置时使用绝对路径。 |

同目录发现提供 `winwincode-worker`、helper 和 helper 清单路径。动作签名密钥仍需部署者提供；当前协议没有让 Server 向 Device 下发该密钥。

Device Client 必须与目标 Server 建立 HTTPS 连接。自签名证书需要转换为 DER，再通过环境变量提供：

```sh
openssl x509 -in /etc/winwincode/server.crt -outform der \
  -out /etc/winwincode/server-root.der
export WWC_DEVICE_TLS_ROOT_DER_FILE=/etc/winwincode/server-root.der
```

选择仅当前用户可读的数据目录。启动 Device Client：

```sh
export WWC_DEVICE_DATA_DIRECTORY="$HOME/.local/share/winwincode/device"
export WWC_DEVICE_SERVER_URL=https://localhost:8443
./bin/wwc device serve \
  --data-dir "$WWC_DEVICE_DATA_DIRECTORY" \
  --server-url "$WWC_DEVICE_SERVER_URL"
```

`wwc device serve` 是前台进程。它会把自身环境传给后续 Worker。发布包中的 `service/` 模板可以交给 `launchd` 或用户级 `systemd`。模板中的占位符必须替换为绝对路径。Linux 模板还要求 `$HOME/.config/winwincode/device-client.env` 提供 `WWC_DEVICE_DATA_DIRECTORY` 和 `WWC_DEVICE_SERVER_URL`。该环境文件也可以作为受支持 Worker 变量的注入点。当前 macOS 模板未预置 Worker 环境变量。

另一个终端可以检查状态：

```sh
./bin/wwc device status --data-dir "$WWC_DEVICE_DATA_DIRECTORY"
./bin/wwc device logs --data-dir "$WWC_DEVICE_DATA_DIRECTORY"
```

首次交换完成后生成一次性连接码：

```sh
./bin/wwc device refresh-code --data-dir "$WWC_DEVICE_DATA_DIRECTORY"
```

明文连接码只显示一次。立即在 Web 端完成连接。锁定 Device Client 会拒绝新的连接验证：

```sh
./bin/wwc device lock --data-dir "$WWC_DEVICE_DATA_DIRECTORY"
./bin/wwc device unlock --data-dir "$WWC_DEVICE_DATA_DIRECTORY"
```

在 Device Client 所在机器注册仓库：

```sh
./bin/wwc repo add /absolute/path/to/repository \
  --data-dir "$WWC_DEVICE_DATA_DIRECTORY"
./bin/wwc repo list --data-dir "$WWC_DEVICE_DATA_DIRECTORY"
```

`wwc init` 和 `wwc repo attach` 只处理本地仓库基线。它们不启动 Server。`wwc repo add --init` 可以初始化尚未成为 Git 仓库的目录。所有 Snapshot 操作都要求显式确认。

### Provider 凭据位置

在 Web 设置页提交 Provider 配置时，浏览器使用 Device Client 公钥加密配置。Server 只转发密文。Device Client 解密后把配置写入：

```text
<WWC_DEVICE_DATA_DIRECTORY>/providers/providers.sqlite3
```

Worker 会读取这个 Device 目录。不要在 Server 上设置 `OPENAI_API_KEY`、`ANTHROPIC_API_KEY` 或其他模型 Provider 密钥。`compose.yaml` 中名为 `model-secrets` 的 Server 卷不代表当前 Device Provider 凭据位置。

实现依据：[`supervisor.rs`](../../crates/winwincode-device-client/src/supervisor.rs) 中的 `SessionSupervisor::launch_process`、`managed_session_config_json` 和 `SessionSupervisor::resolve_worker_binary`；[`main.rs`](../../crates/winwincode-worker/src/main.rs) 中的 `run_worker` 和 `production_codex`；[`device-production-fixture.mjs`](../../scripts/device-production-fixture.mjs) 中的 `establishDeviceOnlyExecutionPath`。fixture 会额外注入 Worker 环境。它不构成生产配置来源。

## 从源码构建

源码构建需要以下固定工具：

| 工具 | 要求 |
| --- | --- |
| Node.js | `.node-version` 为 24.19.0；运行检查接受 24.x。 |
| pnpm | `package.json` 固定为 11.7.0。通过 Corepack 调用。 |
| Rust | `rust-toolchain.toml` 固定为 1.95.0，并包含 `rust-src`、`clippy` 和 `rustfmt`。 |
| Git | Server 仓库操作和来源校验需要。 |

macOS 需要 Apple 编译工具链和 libclang。发布流水线安装 Homebrew `llvm@19`，并设置 `LIBCLANG_PATH` 和 `CLANG_PATH`。

Linux 发布构建支持 Ubuntu 24.04 和 Debian 12 的依赖组合。`scripts/install-code-mode-linux-build-deps.sh` 安装 LLVM 23、Clang 23、libclang、compiler-rt、lld、binutils、`pkg-config`、`libcap-dev`、`libglib2.0-dev` 和 `bubblewrap`。它还准备 Rust V8 构建需要的 compiler-rt 路径。

仓库的 `.cargo/config.toml` 强制 `V8_FROM_SOURCE=1`。首次完整构建会下载并编译 V8。它还使用 `scripts/code-mode-ninja.mjs` 作为 Ninja 入口。构建机需要外网、足够的磁盘空间和较长的编译时间。

```sh
corepack enable
corepack pnpm install --frozen-lockfile
corepack pnpm build
cargo build --locked -p winwincode-cli --bin wwc
```

`pnpm build` 构建 Client、Server、Worker 和内核辅助进程，但不构建 `wwc`。`pnpm start` 只是运行 Server 的开发入口。它仍然要求完整的 Server 环境变量，也不会启动浏览器 Client、Device Client 或 Worker 服务。

源码构建不会生成正式发布签名清单和来源封印。需要可分发制品时使用[发布指南](release.md)中的发布流水线。

## Docker Compose 现状

根目录 `compose.yaml` 是源码部署参考。当前文件不能作为 0.2.0 的可直接启动配置。

已知差异如下：

- `WWC_VERSION` 默认仍是 `0.1.0-alpha.2`。部署 0.2.0 时必须显式设置 `WWC_VERSION=0.2.0`。
- Server 镜像只构建 `winwincode-server`。它不包含 `wwc`、Worker、内核辅助进程或正式发布签名清单。
- Compose 没有设置必填的 `WWC_SERVER_ACTION_SIGNING_KEY_HEX`。
- Compose 设置的 `WWC_SERVER_WORKER_MODE` 和静态远程 Worker 凭据变量不被当前 Server 入口读取。
- 默认绑定地址是 `0.0.0.0:8443`。认证模式因而默认为 `password`。空数据卷没有 Owner，Server 会在监听前失败。
- `repository-data` 是一个新卷。它不会自动包含有效 Git 仓库或与 `WWC_SERVER_CHECKOUT_REVISION` 对应的 tree。
- `model-secrets` 位于 Server。当前模型 Provider 凭据实际保存在 Device Client。

如果继续使用 Compose，先修正这些差异，并为 repository volume 准备精确的 Git 内容。不要只复制 `deploy/compose.env.example` 后执行 `docker compose up`。

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

`control-plane.sqlite3` 和 `device-client.sqlite3` 分别由 `SqliteStorage` 与 `DeviceStore` 的 `initialize_schema` 检查 `user_version`，在同一事务内初始化和校验。版本不匹配时拒绝打开。这个行为不能推广到其他产品数据库。

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

Device 恢复使用相同参数，并把 `--store` 改为 `device`。Device 快照会清零设备凭据。恢复时，命令从目标机器的现有 `device-client.sqlite3` 按 digest 回绑凭据。目标目录没有活的本机凭据、设备身份不同或凭据已经轮换时，恢复会拒绝执行。它不是跨设备克隆机制。

诊断默认只读：

```sh
./bin/wwc backup repair \
  --store server \
  --data-dir /srv/winwincode/server-data
```

`--apply` 只执行白名单动作，例如 SQLite checkpoint 和清理遗留临时文件。每次诊断或修复都会追加 `backup-repair-log.jsonl`。

所有 backup 子命令都支持 `--json`。Server store 未显式传 `--data-dir` 时会读取 `WWC_SERVER_DATA_DIRECTORY`。Device store 始终要求 `--data-dir`。

## 故障定位

- Server 启动即退出：检查所有必填变量、action signing key、TLS 成对配置、Owner 初始化状态和数据 schema。
- `/health` 返回 503：Server 已监听，但内部应用健康检查失败。查看 Server 标准错误输出。
- 浏览器显示启动失败：检查 `client/runtime-config.js` 和 `WWC_SERVER_ALLOWED_ORIGINS`。
- Device Client 拒绝 URL：它要求没有路径、查询和凭据的 `https://` origin。
- Device Client 无法建立自签名 TLS：检查 `WWC_DEVICE_TLS_ROOT_DER_FILE` 是否为 DER 证书。
- 任务提示没有 Provider：在 Web 设置页为该 Device 配置 Provider。不要把密钥放到 Server 环境。
- Worker 在注册前退出：先用 `wwc device logs` 检查缺失的 `WWC_WORKER_*` 变量。未设置 `WWC_WORKER_TLS_ROOT_DER_FILE` 时，Worker 使用 `WWC_DEVICE_TLS_ROOT_DER_FILE`。
- Worker 找不到可执行文件：检查 `wwc` 与 `winwincode-worker` 是否位于同一 `bin` 目录。同目录发现只解决 Worker 可执行文件路径。
- Worker 拒绝 helper：检查 `WWC_WORKER_HELPER_EXECUTABLE` 和 `WWC_WORKER_HELPER_RELEASE_MANIFEST` 是否指向同一已校验发布包中的配对文件。不要绕过签名、摘要、文件模式或 helper 身份校验。
- 数据库提示 unsupported schema version：停止进程。恢复匹配版本的程序和备份。不要手改 `PRAGMA user_version`。

## 相关文档

- [项目说明](../../README.md)
- [安全政策](../../SECURITY.md)
- [测试指南](../testing.md)
- [安全模型](../security-model.md)
- [上游维护](../upstream.md)
- [发布指南](release.md)
