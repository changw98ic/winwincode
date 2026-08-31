# 产品发布证据门禁

阶段 6.7 为同一个源码 commit 生成四个平台的可复核产物证据。门禁只生成和验证文件，不发布包、Git tag 或 GitHub Release。

## 单平台证据

GitHub Actions 的 `Product release matrix` 分别在以下原生 runner 上运行：

| Rust target | runner | 产品文件 |
| --- | --- | --- |
| `aarch64-apple-darwin` | `macos-15` | Server、Worker、Worker 内部 Kernel helper、Client 静态文件 |
| `x86_64-apple-darwin` | `macos-15-intel` | 同上 |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | 同上 |
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | 同上 |

每个 job 使用 Node.js 24、pnpm 11.7.0 和 Rust 1.95.0。`SOURCE_DATE_EPOCH` 必须等于候选 commit 的提交时间。runner 调用：

仓库为发布环境配置 `WINWINCODE_HELPER_RELEASE_PRIVATE_KEY_HEX` secret 和对应的 `WINWINCODE_HELPER_RELEASE_PUBLIC_KEY_HEX` variable。public key 编译进 Server 和 Worker，用于验证随 helper 分发的签名清单；helper 是被签名的对象。private key 只在 runner 内为 helper 清单签名，不写入 artifact 或报告。

```bash
corepack pnpm release:artifact \
  --target TARGET \
  --source-commit SOURCE_COMMIT \
  --source-date-epoch SOURCE_DATE_EPOCH \
  --output release-artifacts
```

命令先运行完整 `pnpm verify`，再在同一个物理 Cargo target 中完成两次冷构建：第一次产物复制到只读快照后，runner 完整删除并重建 target，才开始第二次构建。两次 Rust 二进制和 Client 静态文件的 SHA-256 会逐项比较；macOS 还比较 linker `LC_UUID`。任何差异都会失败。

每个 target 的上传目录固定为：

```text
release-artifacts/TARGET/
  release-artifact-manifest.json
  SHA256SUMS
  bin/winwincode-server
  bin/winwincode-worker
  bin/winwincode-kernel-helper
  bin/winwincode-kernel-helper.release.json
  client/
  legal/LICENSE
  legal/NOTICE
  legal/THIRD_PARTY_NOTICES.md
```

`winwincode-kernel-helper` 是 Worker 的内部随附文件。Local 模式以 `winwincode-server` 为启动入口，通过 `winwincode-local` Rust library 在同一进程组装 Worker。manifest 会记录 Local crate 源码摘要、Cargo.lock 摘要和 `server-local-composition` 模式。

## Manifest 绑定内容

`release-artifact-manifest.json` 至少绑定：

- 完整小写源码 commit、唯一产品版本和 Apache-2.0；
- `SOURCE_DATE_EPOCH`、release source、Cargo/pnpm manifest 与 lock SHA-256；
- Control Plane `winwincode/v1` OpenAPI 和 ExecutionPort v1 schema 的版本与 SHA-256；
- Server、Worker、内部 helper 和每个 Client 静态文件的字节数与 SHA-256；
- helper 签名清单、发布 public key、helper 源码/二进制身份与 Ed25519 验证结果；
- Local 同进程组成身份；
- `LICENSE`、`NOTICE`、`THIRD_PARTY_NOTICES.md`；
- 同一物理 Cargo target 完整清空前后的两次冷构建完全相同。

`SHA256SUMS` 从 manifest 的文件描述符机械生成。手改文件、清单或 checksum 都会失败。

## 汇总四平台报告

下载四个 target artifact，保持目录名不变，然后运行：

```bash
corepack pnpm verify:release-artifacts \
  --expected-commit SOURCE_COMMIT \
  --source-date-epoch SOURCE_DATE_EPOCH \
  --evidence release-artifacts \
  --output release-report.json
```

汇总门禁要求四个 target 恰好各一份，逐文件重算 SHA-256，并确认四个平台使用同一 source、版本、协议、lock、法律文件和完全相同的 Client 静态包。已有 `release-report.json` 内容不同时命令会失败，避免混入旧证据。

上传前，workflow 还会运行单 target artifact security verifier，检查实际 Mach-O/ELF 平台身份、动态库、Client 文件类型、构建路径和凭据泄漏。下载四份证据后再次运行汇总安全检查：

```bash
corepack pnpm verify:release-artifact-security \
  --expected-commit SOURCE_COMMIT \
  --source-date-epoch SOURCE_DATE_EPOCH \
  --evidence release-artifacts \
  --output release-artifact-security-report.json
```

安全报告写到 target 目录外，因此不会改变已经封存的 target 文件集合；它通过 `canonicalEvidenceSha256` 绑定同一份四平台汇总结果。

真实 API→Control Plane→Worker→内嵌 Codex Kernel→Provider→Delivery 纵向、本地同进程模式、分离模式与 Client 远程 Server 直连证据必须绑定相同 commit；它们是阶段 6.7 的并列验收，不用 JSON 声明代替实际运行。
