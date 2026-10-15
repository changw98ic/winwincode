# 发布 WinWinCode

本文说明当前源码的产品发布流程。目标读者是有 GitHub Release 权限的维护者。

`v0.2.0` 已于 2026-10-14 发布。以下版本号用于说明现有流程。准备新发布时，替换为实际确定且未使用的版本号。

## 发布边界

正式发布单位是完整 WinWinCode 产品。Rust workspace crate 和 npm workspace 包都是产品内部模块。模块可以独立构建和测试，但不形成独立公开版本线。

模块划分服务于产品实现、测试和维护。用户通过产品二进制、网页资源和完整发布包使用 WinWinCode。原决定没有记录独立发布方案的比较理由。

当前配置尚未收敛。部分 Rust crate 已设置 `publish = false`，其他 crate 尚未统一设置。根 npm 包是私有包，但部分 npm workspace 包仍保留公开 `publishConfig`。这些配置不改变完整产品这一发布边界。

后续删除未接入产品的模块前，必须核对产品二进制的正常依赖图，并检查测试、schema、脚本和 workspace 清单。后续清理独立 core 发布流程或消费锁时，必须同步处理对应命令、测试和 schema。清理后仍须保留完整产品的来源、版本、摘要和验收机制。

发布配置的目标是第一方 crate 不可发布、内部 npm 包私有，并统一产品版本。模块删除和配置清理由对应任务实施。

## 发布前提

正式发布使用 `.github/workflows/release.yml`。工作流只接受位于 `origin/main` 历史中的 tag commit。tag 必须严格等于 `v<package.json version>`。正式版本必须是三个数字组成的 SemVer，例如 `0.2.0`。该工作流不接受 prerelease 版本字符串。

四个平台的 runner 必须可用：

- `macos-15`，构建 `aarch64-apple-darwin`。
- `macos-15-intel`，构建 `x86_64-apple-darwin`。
- `ubuntu-24.04-arm`，构建 `aarch64-unknown-linux-gnu`。
- `ubuntu-24.04`，构建 `x86_64-unknown-linux-gnu`。

## 准备版本提交

从最新 `main` 创建发布变更。工作树必须干净。

先设置产品版本：

```sh
corepack pnpm version:set 0.2.0
```

`version:set` 会更新：

- 根 `package.json`。
- Client 和产品 npm workspace 的 `package.json`。
- npm 内部依赖的版本范围。
- `Cargo.toml` 的 workspace 版本。
- `Cargo.toml` 中带 `path` 的内部 crate 版本要求。

该命令不更新 `pnpm-lock.yaml` 或 `Cargo.lock`。版本变更后先更新两个 lockfile，再运行固定依赖检查。不要手改生成代码或 lockfile 中的单个包记录。

```sh
corepack pnpm install --lockfile-only
cargo check --workspace
cargo check --workspace --locked
```

为该版本创建发布说明：

```text
.github/release-notes/0.2.0.md
```

发布说明应描述用户可见变化、支持平台、校验方法、macOS 签名状态和数据升级限制。当前 0.2.0 发布说明位于 `.github/release-notes/0.2.0.md`。

运行完整门禁：

```sh
corepack pnpm install --frozen-lockfile
corepack pnpm verify
```

完整门禁的发布 API 检查使用确定性 Device Provider fixture。它验证真实二进制的执行链路和恢复行为。真实 Provider 编程任务质量需要单独执行并留证，方法见 [测试指南](../testing.md)。不要把历史工作流结果或模型自报成功当成本次发布验收。

提交版本、lockfile 和发布说明。合入 `main` 后，确认该 commit 的 `.github/workflows/mainline.yml` push 工作流成功。

仓库提供一个额外的主线来源检查器：

```sh
GH_TOKEN='<read-actions-token>' \
GITHUB_REPOSITORY=owner/repository \
node scripts/verify-mainline-release-source.mjs \
  --source-commit '<40-character-commit>' \
  --default-branch main
```

该脚本要求同仓库、同默认分支、同 commit 恰好有一个成功的 mainline push run。正式 release workflow 当前没有调用这个脚本。维护者需要在打 tag 前单独确认主线结果。

## 创建 tag 并触发发布

在已合入 `main` 的版本 commit 上创建 tag：

```sh
git tag v0.2.0 '<release-commit>'
git push origin v0.2.0
```

push `v*` tag 会自动触发 Product release 工作流。工作流会检查：

- tag 指向当前 checkout commit。
- commit 位于 `origin/main` 历史中。
- `package.json` 版本是正式三段版本。
- tag 精确等于 `v<version>`。

工作流也支持手动触发。`workflow_dispatch` 有两个输入：

| 输入 | 是否必填 | 含义 |
| --- | --- | --- |
| `tag` | 是 | 已存在的 release tag。工作流按该 tag checkout。 |
| `candidate_run` | 否 | 复用某个历史 workflow run 中已封印的四平台候选制品。值必须是数字 run ID。 |

手动触发仍然要求 tag 已存在，并满足同一版本和主线祖先检查。

## 构建和验证流程

每个平台执行以下流程：

1. Checkout release tag，并记录 tag commit 时间为 `SOURCE_DATE_EPOCH`。
2. 安装固定 Node.js 24.19.0 和 Rust 1.95.0。
3. Linux 安装 LLVM 23、Clang 23、lld、libclang、compiler-rt、bubblewrap 和系统开发库。
4. macOS 安装 `llvm@19`，同时使用 Apple 平台工具链。
5. 从源码构建 V8。构建缓存只缓存 `target/**/gn_out`。
6. 构建 Client。
7. 用 `cargo build --locked --release --target <target>` 构建 Server、Worker、内核辅助进程和 `wwc`。
8. 把四个程序设为 `0755`。macOS 对每个程序执行 ad-hoc `codesign --sign -`。
9. 生成 API production source seal。它绑定 tag 源码身份、Server、Worker、`wwc` 和内核辅助进程的摘要。
10. 运行真实发布二进制的 production API 验证，模型响应使用确定性 Device Provider fixture。
11. 保存短期候选制品。
12. 使用工作流自身 commit 上的发布工具进行 staging。
13. 检查原生程序的架构和动态库依赖。
14. 执行凭据内容扫描，并应用与精确二进制绑定的人工复核记录。
15. 生成文件清单、SBOM、包内摘要并归档。

API production 验证在 fixture 场景中要求以下事实成立：

- 执行路径是 `device-worker-only`。
- Device 路径完整。
- Server 初次启动和重启后都处于 `ready`。
- Chat 完成。
- 取消到达 `cancelled`。
- StrongFlow 完成且 verdict 为 `pass`。
- 重复执行内容一致。
- Server 重启前后的消息和交付字节稳定。

这些事实证明发布二进制、Device/Worker 链路和恢复行为。它们不构成真实 Provider 编程质量结论。真实编程任务仍须通过正式 Client 和真实 Provider 另行执行，并记录模型调用、工具结果、候选和独立验证证据。

候选包名为 `candidate-<target>`，保留一天。它包含已验收的四个 `bin` 程序、source seal 和 API 报告。

### 复用候选制品

如果构建和 API 验证已成功，但后续 packaging 或 publish 失败，可以手动重新运行工作流，并把原 run ID 传给 `candidate_run`。

复用时，工作流不会重新构建 Rust 程序。它会下载每个平台的候选包，并恢复二进制、source seal 和原 API 报告。当前工作流会安装 Linux sandbox 运行依赖，并重新运行 production API 验证。新报告会覆盖恢复的报告。后续 staging 还会核对 source seal、二进制摘要、API 报告、原生程序身份、凭据扫描和包内 SHA-256。

只复用同一 tag、同一源码和完整四平台候选的 run。来源不匹配时，source seal 或 staging 检查应拒绝制品。

## 发布扫描例外复核

发布 staging 默认拒绝任何凭据扫描 finding。只有两类文本规则可以进入人工复核：

- `credential.text.assignment`
- `credential.text.provider-token`

复核记录位于：

```text
.github/release-notes/<version>-scan-exceptions.json
```

每条记录必须绑定以下完整身份：

- release source commit。
- target。
- 包内二进制路径。
- 完整二进制 SHA-256。
- finding 的规则和字节位置。
- 人工判断理由。
- 能解释该字节序列的源码路径。

例外只能用于 `bin/winwincode-server`、`bin/winwincode-worker`、`bin/winwincode-kernel-helper` 或 `bin/wwc`。私钥等其他凭据规则不能例外。源码、target、路径、二进制摘要、规则或字节位置发生变化时，原复核记录立即失效。

维护者应先下载候选二进制，核对 finding 附近的可打印字节和对应源码。只有确认它是公开程序字面量时，才能增加复核记录。不要用宽泛模式或原因相似性替代逐个二进制复核。

staging 会把已接受的 finding 写入 `product-release-manifest.json` 的 `reviewedCredentialFindings`。未复核 finding 仍会终止发布。manifest 还记录构建 run、API 复核 run 和最终 producer run，便于区分候选来源与重新验收来源。

## 制品结构

每个平台生成一个压缩包：

```text
winwincode-<version>-<target>.tar.gz
```

压缩包根目录为：

```text
winwincode-<version>-<target>/
├── SHA256SUMS
├── product-release-manifest.json
├── sbom.json
├── bin/
│   ├── winwincode-server
│   ├── winwincode-worker
│   ├── winwincode-kernel-helper
│   └── wwc
├── client/
├── legal/
├── service/
└── verification/
```

`product-release-manifest.json` 记录来源 commit、版本、target、构建和验收 run、人工复核 finding、检查集合和全部文件摘要。检查集合包括原生程序身份检查。包内 `SHA256SUMS` 是完整文件清单。它覆盖包内 manifest 及其余文件。`verification/api-production.json` 是精简的 API 验证结果。`verification/source-seal.json` 是来源封印副本。

publish job 下载四个平台的归档，并校验每个外层 `.tar.gz.sha256`。随后它把四条外层摘要合并为 Release 根目录的 `SHA256SUMS`。这个外层清单只校验四个归档。部署端下载一个平台归档时，应从外层清单精确选择该归档的唯一记录。操作步骤见[部署来源](deploy.md#选择部署来源)。

最终 GitHub Release 至少包含：

- 四个 `.tar.gz`。
- 一个覆盖四个压缩包的 `SHA256SUMS`。

## 发布到 GitHub Releases

四个平台全部成功后，publish job 才会运行。它会：

1. Checkout release tag，以读取对应版本的发布说明。
2. 下载全部四个平台归档。
3. 检查精确的四平台文件集合和外层 SHA-256。
4. 如果 Release 不存在，创建 draft Release。
5. 如果 Release 已存在，只接受仍为 draft 的 Release。
6. 上传所有归档和合并后的 `SHA256SUMS`。
7. 将 Release 切换为公开、正式、latest。

工作流会使用 `--clobber` 覆盖 draft 中同名附件。已公开 Release 不能直接用同一流程覆盖。发现问题时应停止发布、修复源码并使用新版本。不要静默替换已公开的版本内容。

发布完成后，从 GitHub Release 重新下载四个平台文件，并至少检查：

```sh
sha256sum -c SHA256SUMS
tar -xzf winwincode-0.2.0-x86_64-unknown-linux-gnu.tar.gz
cd winwincode-0.2.0-x86_64-unknown-linux-gnu
sha256sum -c SHA256SUMS
./bin/wwc help
```

对 macOS 制品还应检查 `codesign -dv --verbose=4`。结果应是 ad-hoc 签名。当前流程没有 Developer ID 签名和 Apple 公证。

## 本地制品门禁

`release:artifact` 是逐平台的本地证据门禁。它要求干净 checkout 和精确 commit 时间，并对同一平台执行两次隔离构建。它比较 Rust 与 Client 摘要，然后运行 production API 验证。

示例：

```sh
commit=$(git rev-parse HEAD)
epoch=$(git show -s --format=%ct HEAD)

node scripts/run-release-artifact-gate.mjs \
  --target x86_64-unknown-linux-gnu \
  --source-commit "$commit" \
  --source-date-epoch "$epoch" \
  --output release-evidence
```

在原生 runner 上为四个平台分别生成证据目录。收集后的目录必须只包含四个 target 子目录。然后生成矩阵报告：

```sh
node scripts/verify-release-artifacts.mjs \
  --expected-commit "$commit" \
  --source-date-epoch "$epoch" \
  --evidence release-evidence \
  --output release-report.json

node scripts/verify-release-artifact-security.mjs \
  --expected-commit "$commit" \
  --source-date-epoch "$epoch" \
  --evidence release-evidence \
  --output release-security-report.json
```

安全报告检查凭据泄漏、平台和架构、动态库、解释器、rpath 和文件模式。需要检查额外秘密时，可以重复传入 `--sensitive-input <path>`。单平台调查可以传 `--target <target>`。

本地 `verify-release-artifact-security.mjs` 不读取正式 staging 的扫描复核文件。它会保留原始 finding，并在 finding 存在时返回失败。人工复核只在 `stage-product-release.mjs` 的精确二进制绑定流程中生效。

当前本地 `release:artifact` 证据布局与 GitHub Release 最终布局不同。它的 `RUST_RELEASE_ARTIFACTS` 包含 Server、Worker、内核辅助进程和 `wwc`。GitHub workflow 还会增加 service、verification、SBOM 和最终产品 manifest。正式发布以 `.github/workflows/release.yml` 为准。不要把本地 evidence 目录直接上传为产品 Release。

## 失败处理和回滚

- 某个平台构建失败：修复源码或 runner 后重新发布。不要用其他架构制品替代。
- API 验证失败：不能进入 staging。保留日志，但不要发布候选。
- packaging 失败：源码和候选未变化时，可以使用 `candidate_run`。
- Release notes 缺失：在 tag 对应 commit 增加正确文件，重新走版本和 tag 流程。
- 已发布版本有缺陷：发布修复版本。部署端回滚时，恢复旧程序和与旧 schema 匹配的数据备份。

数据库适用边界由 [当前数据库 schema](../invariants.md#当前数据库-schema) 维护。发布说明必须明确已发布用户数据的兼容范围；升级策略尚未定义。回滚操作见 [数据目录与 schema](deploy.md#数据目录与-schema)。

## 相关文档

- [项目说明](../../README.md)
- [安全政策](../../SECURITY.md)
- [测试指南](../testing.md)
- [安全模型](../security-model.md)
- [上游维护](../upstream.md)
- [部署指南](deploy.md)
