# ADR-0027：四平台产物必须绑定同一源码、协议和可复现构建证据

- 状态：已接受
- 日期：2026-08-23
- 阶段 6.7 迁移：2026-08-30
- 单平台入口：[`scripts/run-release-artifact-gate.mjs`](../../scripts/run-release-artifact-gate.mjs)
- 汇总入口：[`scripts/verify-release-artifacts.mjs`](../../scripts/verify-release-artifacts.mjs)
- 合同：[`scripts/release-artifact-contract.mjs`](../../scripts/release-artifact-contract.mjs)
- CI：[`native-release.yml`](../../.github/workflows/native-release.yml)

## 决定

每个支持平台必须在对应的原生 GitHub runner 上，从同一个干净 commit 生成：

- `winwincode-server`；
- `winwincode-worker`；
- Worker 内部使用的 `winwincode-kernel-helper`；
- 绑定 helper 源码、二进制、版本与发布 public key 的 Ed25519 签名清单；
- 与平台无关且四份必须完全相同的 Client 静态文件；
- 项目法律文件、artifact manifest 与 `SHA256SUMS`。

Local 是 `winwincode-server` 通过 `winwincode-local` library 组成的同进程运行模式。发布证据记录 crate 源码和依赖 lock 摘要，并要求该模式通过真实纵向。

## 源码和协议身份

每份 manifest 绑定完整 commit、统一 SemVer、`SOURCE_DATE_EPOCH`、release source digest、Cargo/pnpm manifest 与 lock，以及 Control Plane `winwincode/v1` 和 ExecutionPort v1 的合同摘要。Node 与 Cargo 使用一个产品版本。四个平台使用同一个 helper release public key；private key 只进入签名步骤。

## 可复现性

单平台 runner 使用 `CARGO_INCREMENTAL=0`、Release profile、源码路径 remap 和固定 `SOURCE_DATE_EPOCH`。它在同一个物理 Cargo target 中完成第一次冷构建并封存只读快照，随后完整删除并重建该 target，再完成第二次冷构建。Server、Worker、helper、helper 签名清单与 Client 静态文件必须逐项获得相同 SHA-256；macOS 二进制还必须保留相同的 linker `LC_UUID`。只记录环境变量而没有第二次比较不算通过。

`age 0.11.2` 间接使用的 `i18n-embed-fl 0.9.4` 会把宏参数先放入随机种子的 `HashMap`，导致同一源码生成不同顺序的 LLVM IR。仓库因此只使用 `upstream/vendor/i18n-embed-fl-0.9.4`：它保留 crates.io 源码、MIT 许可证和精确补丁，并把编译期参数改为按 key 排序的 `Vec`；宏生成的运行时值仍是原有 `HashMap`。来源、checksum、上游修复提交和补丁 SHA-256 统一记录在 `upstream/sources.lock.json`，不得同时保留 registry 随机实现。

## 汇总规则

四平台报告要求精确四个 target，逐文件重算 checksum，并确认 source、版本、协议、lock、法律文件和 Client 内容一致。真实 API/Provider/Delivery、本地同进程、分离 Server↔Worker 与 Client 远程 Server 直连由同 commit 的独立运行门禁证明。

## 被替换的合同

阶段 6.7 删除了旧 Host、DSH/Cordis、N-API/native loader 和十个 npm 包发布模型。历史脚本或文档不得作为兼容入口恢复。新门禁只生成证据，不自动发布、打 tag 或创建 GitHub Release。
