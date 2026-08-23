# 更新 Codex Core 与 DeepSeek Harness

WinWinCode 固定一个 Codex Core 源码身份和一个 DeepSeek Harness（DSH）产品外壳身份。每次升级只改变其中一个来源，使失败可以明确归因，也使回滚只影响一组文件。

当前固定值、所需接口、工作区闭包、DSH profile 行和许可义务记录在 [`upstream/sources.lock.json`](../upstream/sources.lock.json)。集成原则见 [ADR 0001](decisions/0001-upstream-integration.md)。

## 共同准备

1. 在独立分支或 worktree 中升级，并记录开始时的完整源码 commit；这是**回滚点 A**。
2. 确认当前固定版本本身通过：

   ```bash
   corepack pnpm install --frozen-lockfile
   corepack pnpm verify:upstream
   corepack pnpm verify:installed-host
   ```

3. 把候选 archive 和展开目录放在仓库外，例如 `/tmp/winwincode-upstream/`。不要在 `third_party/` 中混放多个版本。
4. 候选身份必须包含不可变 tag、完整 40 位 commit 和 archive SHA-256。`main`、`master`、`latest` 或短 commit 不能进入锁文件。

候选 archive 可以按完整 commit 下载：

```bash
mkdir -p /tmp/winwincode-upstream
curl --fail --location \
  --output /tmp/winwincode-upstream/SOURCE-CANDIDATE_COMMIT.tar.gz \
  https://github.com/OWNER/REPOSITORY/archive/CANDIDATE_COMMIT.tar.gz
```

后续检查使用以下占位路径：

```bash
CODEX_ARCHIVE=/tmp/winwincode-upstream/codex-CANDIDATE_COMMIT.tar.gz
CODEX_CANDIDATE=/tmp/winwincode-upstream/codex-CANDIDATE_COMMIT
DSH_ARCHIVE=/tmp/winwincode-upstream/dsh-CANDIDATE_COMMIT.tar.gz
DSH_CANDIDATE=/tmp/winwincode-upstream/dsh-CANDIDATE_COMMIT
```

`verify-upstream-lock.mjs` 会直接核对 archive SHA-256，因此不要手工改写压缩包。

## 更新 Codex Core

Codex 源码随仓库放在 `third_party/codex/`，Rust 工作区直接构建这份源码。升级步骤如下。

### 1. 核对候选

在临时目录查看候选的：

- `LICENSE`、`NOTICE`；
- `codex-rs/Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml`；
- `upstream/sources.lock.json` 所列 `requiredInterfaces` 和 `requiredPublicSymbols`；
- `codex-core-api` 的生产依赖闭包；
- Thread、Plan、Agent Graph、工具、审批、沙箱、Diff、恢复和模型 transport 的变化。

候选要求不同 Rust 版本、移除必要公开接口或改变许可时，先形成单独设计决定，再进入源码替换。

### 2. 替换并记录固定身份

在当前升级 worktree 中完成以下一组变化：

1. 用候选的完整源码替换 `third_party/codex/`；
2. 更新 `upstream/sources.lock.json` 的 Codex tag、version、commit、archive SHA-256、接口、符号和实际生产闭包；
3. 更新 `third_party/codex.UPSTREAM.json` 的同一身份、候选原始 `Cargo.lock` SHA-256 和已应用补丁列表；
4. 逐个检查 `upstream/patches/codex/*.patch`。能删除的补丁直接删除并从锁与元数据移除；仍需要的补丁要针对候选源码重新生成；
5. 从仓库根目录应用每个保留补丁：

   ```bash
   patch --strip=1 \
     --directory=third_party/codex \
     --input=upstream/patches/codex/PATCH_FILE.patch
   ```

6. 更新 `NOTICE` 与 `THIRD_PARTY_NOTICES.md` 中候选实际要求的通知。

上述文件形成**回滚点 B**。在这个点先保留完整差异，不开始 DSH 升级或产品功能修改。

### 3. 运行 Codex 检查

```bash
node scripts/verify-upstream-lock.mjs \
  --codex-root third_party/codex \
  --codex-archive "$CODEX_ARCHIVE"
corepack pnpm verify:upstream
corepack pnpm typecheck
corepack pnpm test
corepack pnpm build:native
corepack pnpm verify:native-package
corepack pnpm verify:installed-host
corepack pnpm verify
```

第一条命令会核对候选身份、archive、许可、Rust 工具链、所需接口、公开符号、生产 crate 闭包、元数据以及补丁确实已经应用。后续命令确认 WinWinCode 仍直接使用内嵌 Codex Core，并检查角色、权限、沙箱、Session 恢复、Plan、Agent Graph、Diff 和 StrongFlow 投影。

### 4. Codex 回滚

任一检查失败时，恢复回滚点 A 中的：

```text
third_party/codex/
third_party/codex.UPSTREAM.json
upstream/sources.lock.json
upstream/patches/codex/
Cargo.toml
Cargo.lock
NOTICE
THIRD_PARTY_NOTICES.md
```

随后重新运行 `corepack pnpm install --frozen-lockfile` 和 `corepack pnpm verify:upstream`，确认基线恢复。不要通过放宽接口清单、闭包清单、补丁检查或测试断言来接纳尚未解释的候选变化。

## 更新 DeepSeek Harness

DSH 通过固定 npm 包、`pnpm-lock.yaml` 和 WinWinCode 的 Cordis profile 进入产品；DSH 源码本身作为候选核对材料，不复制为第二套运行时。

### 1. 核对候选源码

展开候选后，检查：

- 根 `package.json`、`LICENSE`、`THIRD_PARTY_NOTICES.md` 和 `pnpm-lock.yaml`；
- `baseProfile` 与 `webProfile` 的完整行顺序；
- `requiredInterfaces`、`requiredServiceContracts` 和工作区包闭包；
- Chat、Session、Provider、模型、Credential、用户审批、Remote/API 和 UI 插槽的变化；
- `packages/dsh-profile/cordis.patch.yml` 中保留、替换和停用的行是否仍对应候选。

### 2. 更新固定包与 profile

在同一个升级 worktree 中：

1. 更新 `upstream/sources.lock.json` 的 DSH tag、version、commit、archive SHA-256、包闭包、profile 行、接口和许可义务；
2. 更新根 `package.json`、`pnpm-workspace.yaml`、`apps/host/package.json`、`packages/dsh-profile/package.json` 和 `packages/strongflow/package.json` 中直接使用的 DSH 精确版本；
3. 按候选 profile 更新 `packages/dsh-profile/cordis.patch.yml`，继续保留 DSH Chat、Session、模型、Provider、Credential 和审批交互，并把执行交给 Codex Core；
4. 重新生成锁文件：

   ```bash
   corepack pnpm install
   corepack pnpm install --frozen-lockfile
   ```

5. 更新 `NOTICE`、`THIRD_PARTY_NOTICES.md`、直接写死上游身份的检查和 ADR 事实。

这组文件形成 DSH 的**回滚点 B**。

### 3. 运行 DSH 检查

```bash
node scripts/verify-upstream-lock.mjs \
  --dsh-root "$DSH_CANDIDATE" \
  --dsh-archive "$DSH_ARCHIVE"
corepack pnpm verify:upstream
corepack pnpm typecheck
corepack pnpm test
corepack pnpm build
corepack pnpm fixture:delivery
corepack pnpm verify:installed-host
corepack pnpm verify
```

第一条命令核对 archive、版本、许可、包闭包、profile 行和接口。`verify:installed-host` 会在空目录安装实际包并启动 DSH Web，确认 Chat 仍是默认入口、StrongFlow 仍是高级入口，并检查无密钥 Session、人工 Attention、中断和重启恢复。

### 4. DSH 回滚

任一检查失败时，恢复回滚点 A 中的：

```text
upstream/sources.lock.json
package.json
pnpm-workspace.yaml
pnpm-lock.yaml
apps/host/package.json
packages/dsh-profile/package.json
packages/dsh-profile/cordis.patch.yml
packages/strongflow/package.json
NOTICE
THIRD_PARTY_NOTICES.md
```

同时恢复为候选而修改的测试和决策记录，然后运行 `corepack pnpm install --frozen-lockfile`、`corepack pnpm verify:upstream` 和 `corepack pnpm verify:installed-host`。候选 profile 或接口尚未解释清楚时，保留当前固定版本。

## 进入发布前

一个上游升级通过完整 `verify` 后形成**回滚点 C**。随后按 [发布流程](releasing.md) 生成真实模型评估、四个平台原生证据和产品门禁报告。

从回滚点 C 到最终报告之间如果有源码、测试、脚本、文档、上游锁或通知变化，旧评估与发布证据全部重新生成。发布门禁失败时保留报告供排查，回到回滚点 C 修复；若问题来自候选上游本身，则回到 A 并继续使用原固定版本。
