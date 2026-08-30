# 发布 WinWinCode

WinWinCode 以十个 npm 包发布：六个公共产品包和四个单平台原生包。GitHub Actions 为四个 macOS/Linux 目标分别构建原生证据，产品门禁再把它们与当前真实 Delivery 结果绑定到同一源码 commit。

本手册负责版本、候选冻结、发布动作和回滚。证据目录和门禁字段见 [产品发布门禁](release-gate.md)。

## 1. 确定版本与变化

根 `package.json` 是产品版本来源。以下命令会把同一个语义版本写入根清单和全部十个产品包清单：

```bash
corepack pnpm version:set VERSION
```

首个公开候选使用：

```bash
corepack pnpm version:set 0.1.0-alpha.1
```

`winwincode --version` 从已安装的 `apps/host/package.json` 读取版本，因此不需要维护第二个版本常量。Rust crate 不单独发布；它们的内部工作区版本不作为 npm 产品版本。

版本遵循 SemVer：

- 修复现有行为且不改变公开数据结构：patch；
- 增加向后独立的新能力：minor；
- 改变已发布命令、API 或 Delivery 数据结构：major；
- 稳定版前可以使用 `0.x.y` 或 `0.x.y-rc.n`，每个已发布候选仍使用唯一版本。

准备面向用户的发布说明，至少写明新增能力、修复、平台影响、已知限制、上游固定版本和数据迁移。

### Delivery 数据升级

当前源码预览的 Delivery 结构版本是 3。首个稳定版以后，如果本次发布改变该版本，发布候选必须同时包含一条离线迁移路径：上一受支持版本输入、当前版本输出、验证命令和原数据副本回滚点。运行时在迁移后只接受当前结构，不保留跨版本双读、双写或静默回退。

## 2. 冻结候选

在独立发布分支或 worktree 中完成版本和发布说明，记录准备发布的完整小写 commit。该 commit 是所有本地、真实模型和四平台检查的共同身份。

先运行：

```bash
corepack pnpm install --frozen-lockfile
corepack pnpm verify
corepack pnpm fixture:delivery
```

确认根和全部产品包：

- version 完全相同；
- license 为 `Apache-2.0`；
- `LICENSE`、`NOTICE` 和 `THIRD_PARTY_NOTICES.md` 完整；
- 包内容清单不含凭据、日志、依赖目录或本地状态；
- 默认 DSH Chat、高级 StrongFlow、人工方案审核、返工、独立验证和最终人工交付审核均通过。

本地开发检查可以在当前主机完成；四个平台打包是独立发布条件，不阻塞日常功能调试。

## 3. 生成真实 Delivery 结果

按 [真实模型与真实仓库评估](live-evaluation.md) 使用固定仓库、DSH 模型路由、预算和当前候选运行至少一次完成状态的评估。结果必须包含当前 Spec、候选、独立 Reviewer、Verifier、逐项验收结果、最终 Verdict 和人工交付决定。

源码、测试、脚本、文档、评估器、上游锁或通知发生任何变化后，之前的真实结果失效。

## 4. 生成四个平台原生证据

在 GitHub Actions 手动运行 `.github/workflows/native-release.yml`：

| family | runner | Rust target |
| --- | --- | --- |
| macOS arm64 | `macos-15` | `aarch64-apple-darwin` |
| macOS x64 | `macos-15-intel` | `x86_64-apple-darwin` |
| Linux arm64 | `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` |
| Linux x64 | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` |

分别选择 `platform=macos` 和 `platform=linux`。每个 job 必须指向冻结候选的同一 commit，并通过 `scripts/run-native-release-gate.mjs`。

下载四个 artifact 时保持每个目标一个目录。六个公共包会在四个目录重复出现；产品门禁负责核对它们，不要在门禁前覆盖或手工合并。

## 5. 运行产品门禁

按 [产品发布门禁](release-gate.md) 运行：

```bash
corepack pnpm verify:release \
  --expected-commit SOURCE_COMMIT \
  --native-evidence /evidence/native \
  --live-evaluation /evidence/live/RUN_ID/result.json \
  --output /evidence/product-release-gate.json
```

发布批准人检查 `status: passed`、四个目标、相同源码身份、包校验和、真实 Delivery、可重算测量、Apache-2.0 边界和第三方通知。这个命令只写审核报告，不发布 npm、Git tag 或 GitHub Release。

## 6. 发布十个包

从已通过报告引用的四个平台目录中选择 tarball。六个公共包各取一份；四个原生包各取对应目标的一份。再次按报告中的 SHA-256 核对后，按依赖顺序发布：

```text
1. @winwincode/contracts
2. @winwincode/native-darwin-arm64
3. @winwincode/native-darwin-x64
4. @winwincode/native-linux-arm64
5. @winwincode/native-linux-x64
6. @winwincode/native
7. @winwincode/strongflow
8. @winwincode/dsh-profile
9. @winwincode/client
10. winwincode
```

每个包使用已验证 tarball 执行 `npm publish PACKAGE_TARBALL --access public`。不要从工作目录重新打包。发布完成后确认 npm 上十个包的版本、完整性摘要和公开访问状态，再创建与同一源码 commit 对应的 Git tag 和 GitHub Release，并附上发布说明、支持平台、迁移说明及产品门禁报告摘要。

## 7. 发布后检查

在新的空目录安装公开的 `winwincode@VERSION`，运行：

```bash
winwincode --version
winwincode --print-scaffold
winwincode web --no-open --port 3000
```

在至少一个 macOS 和一个 GNU Linux 目标上确认安装到了对应原生包，DSH Chat 是默认入口，StrongFlow 可以主动打开。保存检查版本、目标、时间和结果，不保存凭据或用户 Session 内容。

## 回滚

### 公开发布前

候选尚未发布时，回到冻结前的源码回滚点，删除该候选的本地 tarball、真实评估和四平台 artifact 引用，再运行 `corepack pnpm install --frozen-lockfile` 与 `corepack pnpm verify`。修复后的候选使用新的 commit，所有发布证据重新生成。

### 任一包已经公开后

已经发布的版本保持内容不变。发现产品缺陷时：

1. 停止发布剩余包，并记录哪些包与版本已经公开；
2. 对不应继续安装的版本执行 `npm deprecate PACKAGE@VERSION "REASON"`；
3. 修复源码，使用新的 SemVer 版本重新运行全部门禁；
4. 发布新版本并更新 GitHub Release；
5. 涉及漏洞时按 [安全报告流程](../SECURITY.md) 发布 Security Advisory。

数据迁移失败时停止启用新数据，恢复发布前保留的原数据副本。修复后的迁移随新版本发布，不在已发布包中替换文件。
