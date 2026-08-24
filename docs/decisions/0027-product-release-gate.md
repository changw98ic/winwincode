# ADR-0027：产品发布必须汇总四个平台产物和当前真实交付证据

- 状态：已接受
- 日期：2026-08-23
- 对应任务：`winwincode-9c4.11.5`
- 单平台入口：[`scripts/run-native-release-gate.mjs`](../../scripts/run-native-release-gate.mjs)
- 产品入口：[`scripts/run-product-release-gate.mjs`](../../scripts/run-product-release-gate.mjs)
- 证据检查：[`scripts/product-release-gate.mjs`](../../scripts/product-release-gate.mjs)
- CI：[`native-release.yml`](../../.github/workflows/native-release.yml)
- 验收测试：[`tests/product-release-gate.test.mjs`](../../tests/product-release-gate.test.mjs)
- 操作说明：[`docs/release-gate.md`](../release-gate.md)
- 测量定义：[ADR-0026](0026-explainable-delivery-measures.md)

## 结论

WinWinCode 发布通过两层检查：

```text
每个目标架构的干净 GitHub Actions runner
  ↓
完整源码、测试、Release 原生包和安装检查
  ↓
六个 npm tarball + native-release-evidence.json

四个目标的证据 + 至少一份当前真实模型 result.json
  ↓
产品发布门禁
  ↓
product-release-gate.json
```

第一层必须分别在以下原生 runner 上执行：

| Rust 目标 | runner | 系统家族 |
| --- | --- | --- |
| `aarch64-apple-darwin` | `macos-15` | macOS |
| `x86_64-apple-darwin` | `macos-15-intel` | macOS |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | Linux |
| `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | Linux |

不使用交叉编译结果代替原生运行证明，也不允许只凭当前开发机通过就声明四个平台可发布。

## 单平台检查

`run-native-release-gate.mjs` 在同一进程中按固定顺序执行：

1. 源码和 Rust 格式检查；
2. 源码规则、TypeScript 类型和 Clippy；
3. CPB 只迁移设计的边界检查；
4. 完整 TypeScript 与 Rust 测试；
5. 在独立进程中重复关闭 Testkit 的 DSH Agent、Cordis Context 与内嵌 Kernel，确认关闭后没有文件写入或 Tokio 任务；
6. Codex、DSH 和补丁来源锁检查；
7. 使用固定 Node 与 Rust 版本构建目标原生 Release 包；
8. 原生文件身份、SHA-256、权限和第三方通知检查；
9. 全部发布包文件清单检查；
10. 从 tarball 建立空安装，运行无密钥内核、沙箱和角色权限检查；
11. 从 tarball 启动原始 DSH Chat 与 StrongFlow，检查 CLI、人工审核、中断和恢复；
12. 用 Release 原生包再走一次完整 Delivery：人工要求改方案、失败候选、独立验证失败、人工批准返工、新候选验证和最终批准；
13. 打包六个发布包并写证据。

六个包分别是 `winwincode` Host、contracts、DSH profile、native loader、StrongFlow 和当前平台原生包。每个文件的名称、版本、字节数和 SHA-256 写入 `release-packages.json` 与 `SHA256SUMS`。

`native-release-evidence.json` 绑定：

- 完整 Git commit、项目版本和 Apache-2.0 项目许可证；
- 项目源码摘要、pnpm/Cargo/toolchain/upstream lock 摘要；
- Codex 与 DSH 固定 commit；
- GitHub Actions workflow、run、runner OS 和处理器；
- Release 原生 `build-info.json` 与二进制身份；
- 六个 tarball；
- 完整确定性交付测量；
- 已执行的固定检查清单。

证据只能由 GitHub Actions 环境生成；产品门禁同时重算本地源码和所有可下载文件摘要。它不把一个 JSON 字段当成密码学签名，正式托管时仍应保留 GitHub run、artifact retention 和平台保护规则。

## 产品级检查

`run-product-release-gate.mjs` 要求：

- 四个目标各有且只有一份当前 commit 的原生证据；
- macOS 与 Linux 两个系统家族都存在；
- 每份证据包含完整固定检查，没有删项；
- tarball、`release-packages.json` 和 `SHA256SUMS` 仍与记录完全一致；
- 根项目和全部发布包只声明 Apache-2.0，根 `LICENSE`、`NOTICE` 和 `THIRD_PARTY_NOTICES.md` 存在；
- 当前源码仍满足 CPB 只迁移设计的边界；
- 至少一份真实 DSH 提供商运行已经完成，使用当前源码、当前评估器和四个已证明目标之一；
- 真实运行的完整 Delivery、当前 Spec revision、候选、Evidence 和 Verdict 关系有效；
- 保存的五组测量可以重算，完整度通过、可信度得到独立支持、用量完整，没有开放阻塞 Attention，也没有误报成功或误报失败。

门禁不要求总分。稳定性可能显示一次有证据的返工；这比隐藏返工并给出一个高分更有用。任何缺少目标、过期源码、被改写 tarball、过期候选、变化后的测量或不完整人工决定都会使命令以非零状态结束。

## 源码身份

发布源码摘要覆盖项目自有的应用、包、Rust crate、脚本、测试、文档、CI、补丁和根配置。`dist`、`prebuild`、`target`、依赖目录和固定上游源码副本不重复进入摘要；上游通过 `sources.lock.json`、`codex.UPSTREAM.json`、补丁摘要和各自 commit 表示。

真实模型评估也保存同一个项目源码摘要。因此在真实评估后修改门禁、测试、文档或产品源码都会让旧结果失效，必须重新评估，而不是把旧成功记录套在新源码上。

## 非目标

门禁不会：

- 发布 npm 包、创建 Git tag 或 GitHub Release；
- 安装或调用外部编程 Agent；
- 读取 CPB 作业、队列、日志、配置或其他内部数据；
- 复制 Codex 自己的 Agent 调度、工具或沙箱；
- 用一个主观总分决定发布。

通过只表示证据集合满足当前发布要求。实际发布仍是随后单独批准的远端操作。

## 回滚

门禁只读取源码和证据，并写一个本地报告，不修改 Delivery、Git 历史或远端包。失败时删除或保留失败报告均不影响产品数据；修复缺失检查后重新生成对应平台证据和真实评估。已经发布的版本仍按包仓库的正常撤回或发布后续修复版本处理，不能靠改写这份报告伪造回滚。
