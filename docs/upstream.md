# 上游源码与补丁

本文说明 WinWinCode 0.2.0 的第三方源码、来源锁、补丁和验收方式。许可证全文与署名见 [`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md)。通用检查见[测试指南](testing.md)。产品边界见[架构总览](architecture/overview.md)、[契约](contracts.md)和[安全模型](security-model.md)。目标作用域见 [ADR-0004](adr/0004-scope-model.md)。

## 事实来源

| 路径 | 作用 |
| --- | --- |
| `upstream/sources.lock.json` | 锁定项目工具链、Codex 上游、vendored Cargo 源码、全部补丁、摘要和许可证义务。 |
| `third_party/codex.UPSTREAM.json` | 锁定 `third_party/codex` 对应的 Codex 身份、归档摘要、补丁应用顺序和打补丁前的 `Cargo.lock` 摘要。 |
| `crates/kernel/src/lib.rs` 的 `CODEX_COMMIT`、`CODEX_TAG`、`CODEX_PATCH_SET` | 把运行时使用的 Codex 版本和补丁清单编入产品。 |
| `upstream/dsh-frontend.json` | 逐文件记录改编自 DeepSeek Harness 的前端来源、目标路径、上游摘要和适配说明。 |

Codex 补丁顺序以 `third_party/codex.UPSTREAM.json` 的 `patchesApplied` 为准。补丁的统一登记与摘要以 `upstream/sources.lock.json` 的 `patches` 为准。

## 内嵌 Codex

当前源码树位于 `third_party/codex`。锁定身份如下：

- 仓库：`https://github.com/openai/codex`
- 标签：`rust-v0.149.0`
- 版本：`0.149.0`
- 提交：`758ef40f50c1a458425c7cfbf1eb12cbc07af0b0`
- 上游归档 SHA-256：`0413a0e7680bcc2b6c6e998a6ad358115707317ef5d0121dcb9275e88c36121a`
- 许可证：Apache-2.0

`upstream/patches/codex/` 保存 119 个产品补丁。编号覆盖 `0001` 至 `0120`，其中没有 `0004`。`third_party/codex.UPSTREAM.json` 的 `patchesApplied` 也有 119 项。其顺序与 `upstream/sources.lock.json` 中已应用的 Codex 补丁一致。

全部组件共有 121 个补丁。另两个补丁是：

- `upstream/patches/i18n-embed-fl/0001-stable-specified-argument-order.patch`
- `upstream/patches/rusqlite/0001-quote-savepoint-identifiers.patch`

当前 121 个登记项的 `planned` 都是 `false`。`upstream/sources.lock.json` 还记录补丁目标和 `patchSha256`。需要非默认路径剥离层数时，条目记录 `stripComponents`。

`crates/kernel` 是第一方代码与 Codex Core 的封装边界。它通过 `codex-core-api` 使用 `CodexThread`、`ThreadManager`、`ConfigBuilder`、`Op` 和协议类型。`crates/helper` 构建 `winwincode-kernel-helper`。该辅助进程承载 Code Mode 主机入口和受控的本地执行能力。

## 补丁重放

仓库保存打好补丁的 Codex 源码树。补丁重放由测试实现。

文本补丁使用以下参数：

```text
patch --batch --forward --fuzz=0 --strip=<stripComponents> --directory <directory>
patch --batch --reverse --fuzz=0 --strip=<stripComponents> --directory <directory>
```

`stripComponents` 默认是 `1`。二进制补丁 `upstream/patches/codex/0115-app-server-internal-error-schema.patch` 使用 `git apply --binary`，反向操作再加 `--reverse`。

四个 Codex 重放测试可一起运行：

```sh
node --test \
  tests/code-mode-patch-replay.test.mjs \
  tests/code-mode-dispatch-patch-replay.test.mjs \
  tests/codex-handoff-patch-replay.test.mjs \
  tests/codex-host-model-patch-replay.test.mjs
```

这些测试执行以下检查：

- 比较 `upstream/sources.lock.json` 与 `third_party/codex.UPSTREAM.json` 的上游身份。
- 比较 119 个 Codex 补丁的顺序。
- 校验已记录的补丁摘要。
- 对各测试覆盖的原始目标文件先反向还原，再正向重放。
- 要求重放结果与 `third_party/codex` 中的当前字节一致。
- 对新增文件检查反向重放后的缺失状态。

这四个测试覆盖其文件中列出的目标集合。它们不等同于重新下载并逐字节核对整个上游归档。归档身份仍由锁文件中的 `archiveSha256` 固定。

## 其他 vendored 源码

`upstream/vendor/i18n-embed-fl-0.9.4` 和 `upstream/vendor/rusqlite-0.39.0` 由根 `Cargo.toml` 的 `[patch.crates-io]` 接入。每个条目在 `vendoredCargoSources` 中记录 crates.io 校验和、上游提交、补丁前后源码树摘要、许可证文件和补丁摘要。

定向验收命令如下：

```sh
node --test tests/i18n-embed-fl-reproducibility.test.mjs
node --test tests/rusqlite-savepoint-backport.test.mjs
```

前一个修复已进入 `i18n-embed-fl` 0.10.0。后一个修复已进入 `rusqlite` 0.40.1。升级到含修复的版本后，才可删除对应 vendored 源码和补丁。

## DeepSeek Harness 前端来源

`upstream/dsh-frontend.json` 锁定 DeepSeek Harness `dsh-v0.1.0-rc.8`、提交 `141eb6fef83422698aef7a981029e843e8161534`。该文件逐项记录来源路径、WinWinCode 目标路径和上游 SHA-256。

当前产品包含改编后的前端代码。主要目标位于 `apps/client/src/dsh/`、`apps/client/src/styles/features/`、`apps/client/src/preferences.ts`、`apps/client/src/styles/tokens.css`、`apps/client/src/styles/components.css` 和 `apps/client/src/styles/shell.css`。

`upstream/sources.lock.json` 的 `historicalSources` 另把 DeepSeek Harness 记录为 `historical-attribution-only`。这表示原上游源码包不是当前 workspace 依赖，也不作为完整源码树分发。它不撤销改编前端文件的 MIT 署名义务。

相关检查是：

```sh
node --test tests/dsh-bundle.test.mjs tests/open-source-governance.test.mjs
```

## 许可证

WinWinCode 项目使用 Apache-2.0。以下文件必须保留并随发布源边界处理：

- 根 `LICENSE`、`NOTICE`、`THIRD_PARTY_NOTICES.md`
- `third_party/codex/LICENSE`、`third_party/codex/NOTICE`
- `upstream/vendor/i18n-embed-fl-0.9.4/LICENSE.txt`
- `upstream/vendor/rusqlite-0.39.0/LICENSE`

`scripts/release-source-contract.mjs` 的 `verifyReleaseLegalBoundary` 检查项目许可证和根法律文件。`tests/open-source-governance.test.mjs` 检查法律边界与 DeepSeek Harness 署名。

## 修改和升级规则

第三方修改必须同时具备补丁、锁定记录和验收。不要只修改 `third_party/codex` 或 `upstream/vendor/` 中的结果文件。

升级步骤如下：

1. 固定新上游标签、提交和归档 SHA-256。
2. 替换上游源码树。
3. 逐个移植补丁。保持补丁序号和应用顺序明确。
4. 更新 `upstream/sources.lock.json`。
5. 更新 `third_party/codex.UPSTREAM.json`。
6. 更新 `crates/kernel/src/lib.rs` 的 Codex 常量。
7. 更新许可证和署名文件。
8. 运行相关重放测试、`pnpm lint:source`、Rust 构建和受影响测试。

补丁必须能以 `--fuzz=0` 还原和重放。新增或修改目标文件时，要更新重放测试中的上游原始摘要。第三方源码、vendored 源码和补丁保留上游注释与许可证信息。第一方源码只保留 SPDX 许可证头；它是第一方零注释规则的唯一豁免。详情见 [ADR-0001](adr/0001-source-code-comments.md)。
