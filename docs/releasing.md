# 发布 WinWinCode

当前 alpha 候选由一个 TypeScript Client 静态包、Rust Server、Rust Worker、Worker 内部 Kernel helper，以及 Server 内的 Local 同进程组成模式构成。

## 1. 统一版本

根 `package.json` 是命令入口，Node 产品 manifest 与 Cargo workspace 必须使用同一个 SemVer：

```bash
corepack pnpm version:set 0.1.0-alpha.1
cargo metadata --format-version 1 >/dev/null
corepack pnpm install --lockfile-only
```

提交前检查 `package.json`、Client/contracts/StrongFlow package manifest、`Cargo.toml [workspace.package].version` 和 `Cargo.lock` 中项目 crate 版本完全一致。lock 只能由 Cargo/pnpm 根据 manifest 生成。

## 2. 冻结候选

候选 commit 必须是完整 40 位小写 SHA。记录它的提交时间作为唯一 `SOURCE_DATE_EPOCH`：

```bash
SOURCE_COMMIT=$(git rev-parse HEAD)
SOURCE_DATE_EPOCH=$(git show -s --format=%ct "$SOURCE_COMMIT")
corepack pnpm install --frozen-lockfile
corepack pnpm verify
```

`pnpm verify` 在当前 checkout 中依次执行源码格式与边界、TypeScript 构建与测试，以及 Rust 构建、Clippy、全 workspace 测试、产品检查和 direct API。TypeScript 和 Rust 产品在各自检查中只构建一次。普通 CI 对同一提交并行执行这三组检查，再汇总成一个 `Canonical workspace verification` 结果；pull request 与默认分支 push 不会为同一源提交重复执行等价全量任务。

候选必须保持干净；源码、manifest、lock、协议、测试、脚本、文档或通知变化后，旧 artifact 全部失效。

四平台 workflow 使用仓库 secret `WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX` 和对应 repository variable `WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX` 为 Worker 内部 helper 生成可验证的签名清单。批准人应先确认两者属于当前发布身份且相互匹配。

## 3. 四平台 artifact

feature 分支先通过普通 CI 和合并门；合并到默认分支后，默认分支 exact commit 的 `Mainline verification` push run 也必须成功。随后从默认分支手动启动一次 `Product release matrix`，把完整 40 位候选 SHA 填入唯一的 `source_commit` 输入。轻量 source verifier 通过只读 GitHub Actions API 确认同一仓库、默认分支、同一 SHA、push 事件、成功结论且匹配记录唯一；pull request、feature 分支、fork、失败 run、另一个 SHA 或重复记录都会在发布 job 取得签名 secret 前失败。发布 workflow 不重新执行 `pnpm verify`，也不读取分支当前 HEAD。

四个原生 target job 只构建 Server、Worker、内部 helper 和同一份 Client 静态包，在同一物理 Cargo target 完整清空前后执行两次冷构建比较，并用 release 二进制运行一次 direct API 完整流程；它不运行 clean checkout、全 workspace Rust 测试或压力循环。详细层级、目录与命令见 [产品发布证据门禁](release-gate.md)。

Rust 文件名固定为 `winwincode-server`、`winwincode-worker` 和 `winwincode-kernel-helper`；Local 组成使用 `winwincode-local` library。

四个平台必须是：

- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `aarch64-unknown-linux-gnu`
- `x86_64-unknown-linux-gnu`

只下载四个产品 artifact，并把每份内容直接放入同名 target 目录：

```bash
set -euo pipefail
RUN_ID=RUN_ID
rm -rf release-artifacts release-security-reports
mkdir -p release-artifacts release-security-reports
for TARGET in \
  aarch64-apple-darwin \
  x86_64-apple-darwin \
  aarch64-unknown-linux-gnu \
  x86_64-unknown-linux-gnu
do
  gh run download "$RUN_ID" --name "$TARGET" --dir "release-artifacts/$TARGET"
  gh run download "$RUN_ID" --name "release-security-$TARGET" --dir "release-security-reports/$TARGET"
done
```

`release-artifacts/` 的一级目录因此精确为四个 Rust target，可直接交给汇总命令。`release-security-reports/` 与产品 evidence root 分离，不会向待封存的产品目录加入额外文件。

## 4. 汇总与运行证据

下载四份 artifact 后运行 `pnpm verify:release-artifacts` 和 `pnpm verify:release-artifact-security`。发布批准人检查：

1. 四个平台的 source commit、版本、协议与 lock 完全一致；
2. Server、Worker、内部 helper、helper 签名清单、Client 静态文件和法律文件的 SHA-256 可重算；
3. Client 的 `runtime-config.js` 只通过 `serverUrl` 指向 Server；
4. Local 模式以 `winwincode-server` 为启动入口，由 `winwincode-local` library 完成同进程组成；
5. 同 commit 的本地/分离 API 纵向、Client 远程直连、重启、取消和 Delivery terminal 证据通过；
6. artifact 不含凭据、日志、数据库、依赖目录或构建 target。

## 5. 发布与回滚

阶段 6.7 的脚本只写证据，不自动发布。远端发布必须使用已经验证的原始 artifact，发布后保存 GitHub run、target、manifest SHA-256、报告 SHA-256 和同一 commit。

候选发布前失败时，丢弃该 commit 的四平台 artifact，修复后用新 commit 和新 `SOURCE_DATE_EPOCH` 重跑。已经公开的版本保持字节不变；修复使用新的 SemVer，不覆盖旧 artifact。
