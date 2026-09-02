# 更新 Codex Core 与 vendored Cargo 上游

WinWinCode 当前固定两类上游输入：`third_party/codex/` 中直接构建的 Codex Core，以及 `upstream/vendor/` 中由根 Cargo workspace 精确选择的第三方源码。当前身份、生产闭包、补丁、checksum 和许可义务统一记录在 [`upstream/sources.lock.json`](../upstream/sources.lock.json)；集成原则见 [ADR-0001](decisions/0001-upstream-integration.md)。

每次只更新一个上游来源。不要把上游升级、产品功能修改和发布脚本修改放进同一个验证窗口。

## 共同准备

1. 在独立分支或 worktree 中升级，记录开始时的完整 commit 和当前 `releaseSourceSha256`；这是**回滚点 A**。
2. 确认当前固定源码通过：

   ```bash
   corepack pnpm install --frozen-lockfile
   cargo metadata --locked --offline --format-version 1
   corepack pnpm verify
   ```

3. 把候选 archive 和展开目录放在仓库外，例如 `/tmp/winwincode-upstream/`。不要在 `third_party/` 或 `upstream/vendor/` 中并排保留两个版本。
4. 候选身份必须包含不可变版本或 tag、完整 commit（适用时）和取得 archive 的 SHA-256。`main`、`master`、`latest`、版本范围或短 commit 不能进入机器可读清单。
5. 保存当前上游目录、对应补丁、manifest、lock、NOTICE 和 `THIRD_PARTY_NOTICES.md` 的差异，确保回滚可以一次恢复。

示例下载使用占位值，不把本机路径写进仓库：

```bash
mkdir -p /tmp/winwincode-upstream
curl --fail --location \
  --output /tmp/winwincode-upstream/SOURCE-CANDIDATE_COMMIT.tar.gz \
  https://github.com/OWNER/REPOSITORY/archive/CANDIDATE_COMMIT.tar.gz
shasum -a 256 /tmp/winwincode-upstream/SOURCE-CANDIDATE_COMMIT.tar.gz
```

## 更新 Codex Core

Codex 源码位于 `third_party/codex/`，Rust Kernel 直接构建它，不经过命令行回退或独立 app-server。

### 1. 核对候选

在仓库外展开候选并检查：

- `LICENSE`、`NOTICE`；
- `codex-rs/Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml`；
- `upstream/sources.lock.json` 当前记录的 `requiredInterfaces` 和 `requiredPublicSymbols`；
- 从 `codex-core-api` 出发的实际生产 crate 闭包；
- Thread、Turn、Plan、Agent Graph、工具、审批、沙箱、Diff、恢复和模型 transport 的变化。

候选改变 Rust 工具链、移除必要公共接口、扩大网络或执行权限、改变许可证时，先形成单独设计决定，再替换源码。

### 2. 替换并记录固定身份

1. 用候选完整替换 `third_party/codex/`；
2. 更新 `upstream/sources.lock.json` 的 tag、version、commit、archive SHA-256、接口、符号和生产闭包；
3. 更新 `third_party/codex.UPSTREAM.json` 的同一身份、候选原始 Cargo lock SHA-256 和已应用补丁列表；
4. 逐个审查 `upstream/patches/codex/*.patch`。上游已包含的修复应删除对应补丁与记录；仍需要的补丁针对候选重新生成；
5. 从仓库根目录精确应用保留补丁：

   ```bash
   patch --batch --strip=1 \
     --directory=third_party/codex \
     --input=upstream/patches/codex/PATCH_FILE.patch
   ```

6. 通过 Cargo 规范命令更新根 `Cargo.lock`，不要手改 lock；
7. 更新 `NOTICE`、`THIRD_PARTY_NOTICES.md` 和直接记录上游事实的 ADR/测试。

这组文件形成**回滚点 B**。

### 3. 验证 Codex 候选

```bash
cargo metadata --locked --offline --format-version 1
cargo check --workspace --all-targets --all-features --locked --offline
cargo clippy --workspace --all-targets --all-features --locked --offline -- -D warnings
corepack pnpm verify
```

另外逐项比较 `upstream/sources.lock.json` 与 `third_party/codex.UPSTREAM.json` 的 commit、版本、原始 lock、补丁列表和许可证。生产闭包、公共接口或补丁锚点发生未解释变化时停止升级。

### 4. 回滚 Codex

任一检查失败时，从回滚点 A 一次恢复：

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

随后重新运行 frozen install、locked/offline metadata 和完整 `pnpm verify`。不要用放宽闭包、接口、补丁或安全断言的方式接纳候选。

## 更新 vendored Cargo source

当前 vendored Cargo source 是 `i18n-embed-fl 0.9.4`。它用于修复上游 proc macro 的非确定顺序，根 `[patch.crates-io]` 只能选择一个 path source。

### 1. 取得并核对候选

1. 从 crates.io 下载精确 `.crate` archive，在仓库外计算 SHA-256；
2. 核对 crate 名称、版本、repository、Cargo manifest、许可证文件和源码树；
3. 把未经修改的源码树摘要记录为 `upstreamSourceTreeSha256`；
4. 确认上游修复 commit 或目标版本，不根据版本号猜测行为已经修复。

### 2. 应用最小补丁

1. 只在 `upstream/vendor/PACKAGE-VERSION/` 保留一个完整候选源码树；
2. 把最小补丁写入 `upstream/patches/PACKAGE/`；
3. 用非交互命令应用：

   ```bash
   patch --batch --strip=1 \
     --directory=upstream/vendor/PACKAGE-VERSION \
     --input=upstream/patches/PACKAGE/PATCH_FILE.patch
   ```

4. 记录补丁、补丁后源码树、关键源码文件和许可证文件的 SHA-256；
5. 更新根 `[patch.crates-io]` 后，通过 Cargo 规范命令刷新唯一 lock 项：

   ```bash
   cargo update --offline -p PACKAGE --precise VERSION
   ```

6. 更新 `upstream/sources.lock.json`、`THIRD_PARTY_NOTICES.md`、ADR 和精确回归测试。

完成替换和 lock 生成后形成该来源的**回滚点 B**。

### 3. 验证 vendored source

```bash
cargo metadata --locked --offline --format-version 1
node --test tests/i18n-embed-fl-reproducibility.test.mjs
corepack pnpm test
corepack pnpm verify
```

`i18n-embed-fl` 回归会检查唯一 path source、registry archive checksum、原始与补丁后源码树、补丁反向恢复、许可证、宏生成顺序和 release source inventory。Cargo metadata 必须只返回一个目标版本；根测试还会通过实际下游构建使用该 proc macro。

### 4. 回滚 vendored source

失败时从回滚点 A 一次恢复：

```text
Cargo.toml
Cargo.lock
upstream/vendor/PACKAGE-VERSION/
upstream/patches/PACKAGE/
upstream/sources.lock.json
THIRD_PARTY_NOTICES.md
docs/decisions/0001-upstream-integration.md
```

恢复后重新运行 locked/offline metadata、精确上游测试和 `corepack pnpm verify`。

## 进入发布前

一个上游升级通过完整验证后形成**回滚点 C**。随后按[发布流程](releasing.md)在同一 release source digest 上生成四平台产物。

从回滚点 C 到最终报告之间，只要源码、测试、脚本、文档、上游锁、补丁或通知发生变化，旧产物和报告就不再代表当前源码，必须重新构建。发布失败时保留报告供排查；若问题来自候选上游，则回到 A，不保留双版本或长期兼容路径。
