# 产品发布门禁

这个流程用于生成“当前源码和四个平台产物可以进入发布审批”的证据。它不会上传 npm 包、创建 Git tag 或发布 GitHub Release。

## 1. 生成四个平台证据

在 GitHub Actions 中手动运行 `Native release matrix` 两次：

1. `platform=linux`，生成 Linux arm64 与 x64 两个 artifact；
2. `platform=macos`，生成 macOS arm64 与 x64 两个 artifact。

每个 job 使用固定 Node.js 24.19.0 和 Rust 1.95.0，在目标原生 runner 上执行完整发布检查。成功 artifact 包含：

```text
native-release-evidence.json
release-packages.json
SHA256SUMS
winwincode-*.tgz
winwincode-contracts-*.tgz
winwincode-dsh-profile-*.tgz
winwincode-native-*.tgz
winwincode-strongflow-*.tgz
winwincode-native-PLATFORM-*.tgz
```

下载四个 artifact，保持每个目标一个独立目录。不要把同名的公共包 tarball 覆盖到一个目录。

## 2. 生成当前真实模型结果

按 [`docs/live-evaluation.md`](live-evaluation.md) 运行至少一次真实 DSH 提供商评估。使用的 WinWinCode 源码必须与四个平台 job 完全相同。最终 `result.json` 必须是 `completed`，并且 Delivery 已通过独立 Reviewer、Verifier 和人工交付审核。

真实评估后发生任何项目源码、测试、脚本、文档或 CI 变化，旧结果都会因源码摘要不同而失效。此时重新运行真实评估。

## 3. 准备本地检查环境

在四个平台 job 使用的同一个 commit 上安装和构建：

```bash
corepack pnpm install --frozen-lockfile
corepack pnpm build:ts
```

`build:ts` 是必需的，因为门禁会核对真实评估实际执行的测量模块文件。

目录示例：

```text
/evidence/native/aarch64-apple-darwin/native-release-evidence.json
/evidence/native/x86_64-apple-darwin/native-release-evidence.json
/evidence/native/aarch64-unknown-linux-gnu/native-release-evidence.json
/evidence/native/x86_64-unknown-linux-gnu/native-release-evidence.json
/evidence/live/RUN_ID/result.json
```

## 4. 运行产品门禁

把 `SOURCE_COMMIT` 替换成四个 GitHub Actions job 记录的完整小写 commit：

```bash
corepack pnpm verify:release \
  --expected-commit SOURCE_COMMIT \
  --native-evidence /evidence/native \
  --live-evaluation /evidence/live/RUN_ID/result.json \
  --output /evidence/product-release-gate.json
```

通过时标准输出包含：

- `status: passed`；
- 精确源码 commit；
- 四个 Rust 目标；
- 使用的真实评估 run ID；
- 最终报告 SHA-256 和文件位置。

同一输入重复运行会复用字节完全相同的输出。已有输出内容不同会失败，避免新旧证据被静默混合。

门禁在读取证据、真实评估和 tarball 后、生成通过报告前运行 Credential 泄漏扫描。扫描会展开 gzip/tar、检查 JSON 字段策略、已知 Provider 凭据编码和显式秘密指纹；命中时只报告文件与规则，不回显匹配值。损坏或不支持的压缩条目按失败处理。

## 5. 人工审核报告

发布批准人至少检查：

1. `source.commit` 是准备发布的 commit；
2. `nativeTargets` 恰好包含四个支持目标，且 CI runner 与目标对应；
3. 每个目标有六个 tarball 和独立确定性 Delivery 结果；
4. `evaluations.live` 指向当前候选、Spec revision、Verdict 和可重算测量；
5. `falseSuccessRisk` 与 `falseFailureRisk` 均为 `false`；
6. `boundaries` 显示内嵌 Codex Core、DSH 外壳、WinWinCode Delivery、Apache-2.0，且不依赖外部编程 Agent 或 CPB runtime。

## 常见失败

| 错误 | 具体含义 |
| --- | --- |
| `NATIVE_MATRIX_INCOMPLETE` | 四个目标中有缺失、重复或混入其他目标 |
| `SOURCE_MISMATCH` | 平台证据来自另一个 commit 或另一份源码 |
| `ARTIFACT_MISMATCH` | tarball、清单或 SHA256SUMS 在 CI 后被改变 |
| `CHECK_MISSING` | 单平台 job 没有完成当前固定检查集合 |
| `LIVE_EVALUATION_STALE` | 真实评估使用的源码、评估器或原生目标已经过期 |
| `LIVE_EVALUATION_FAILED` | Delivery、人工决定、用量或最终结论没有完整通过 |
| `EVALUATION_MISMATCH` | 保存的测量不能从原始结果重新算出 |
| `LEGAL_BOUNDARY_FAILED` | 项目许可证或必要第三方通知不完整 |
| `DESIGN_BOUNDARY_FAILED` | 当前源码重新引入了已排除的 CPB 运行依赖或状态路径 |
| `CREDENTIAL_LEAK_DETECTED` | 证据、真实评估或发布包命中 Credential 泄漏门禁 |

失败报告区分产物、源码、真实评估和法律边界。修复对应事实并重新生成证据；不要手改 evidence JSON 或最终报告。
