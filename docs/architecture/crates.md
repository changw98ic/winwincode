# WinWinCode crate 清单

本文记录 WinWinCode 0.2.0 当前的第一方 Rust crate。依赖关系来自 `cargo metadata --no-deps --format-version 1 --offline`。这里只列工作区内的 Cargo normal 依赖和反向依赖。开发依赖不计入。依赖列表省略统一的 `winwincode-` 前缀。

架构关系见 [架构总览](overview.md)。公共术语见 [术语表](../glossary.md)。协议边界见 [契约](../contracts.md)。测试分层见 [测试策略](../testing.md)。所有 workspace crate 都是产品内部模块。发布规则见[发布边界](../operations/release.md#发布边界)。

## 运行入口

| 产品名称 | Cargo 包 | 入口 |
| --- | --- | --- |
| Server | `winwincode-server` | `crates/winwincode-server/src/main.rs` |
| Worker | `winwincode-worker` | `crates/winwincode-worker/src/main.rs` |
| kernel-helper | `winwincode-kernel-helper` | `crates/helper/src/main.rs` |
| CLI `wwc` | `winwincode-cli` | `crates/winwincode-cli/src/main.rs` |
| Local | `winwincode-local` | `crates/winwincode-local/src/lib.rs::LocalLauncher` |

## `winwincode-domain`

职责：提供生成的标识和值对象。当前 `RepositoryScope` 保留 organization、workspace、project、repository 四级作用域。

- 主要入口：`crates/winwincode-domain/src/lib.rs` 和 `crates/winwincode-domain/src/generated.rs`。
- normal 依赖：无第一方 crate。
- 反向依赖：`execution-port`、`api`、`audit`、`cli`、`delivery`、`storage`、`publication`、`provider`、`worker`、`codex`、`control-plane`、`session`、`server`、`test-assets`。
- 测试：`crates/winwincode-domain/tests`，1 个 Cargo test target。

## `winwincode-api`

职责：提供生成的公共 HTTP 和 WebSocket 类型。

- 主要入口：`crates/winwincode-api/src/lib.rs`；生成类型位于 `src/generated.rs`。
- normal 依赖：`winwincode-domain`。
- 反向依赖：`device-client`、`client-port`、`provider`、`control-plane`、`server`。
- 测试：`crates/winwincode-api/tests`，3 个 Cargo test target。

## `winwincode-execution-port`

职责：提供 Control Plane 与 Worker 的协议、传输帧、动作规范化和动作授权类型。

- 主要入口：`crates/winwincode-execution-port/src/lib.rs`；生成协议位于 `src/generated.rs`。
- normal 依赖：`winwincode-domain`。
- 反向依赖：`kernel`、`storage`、`provider`、`worker`、`codex`、`control-plane`、`server`。
- 测试：`crates/winwincode-execution-port/tests`，26 个 Cargo test target。

## `winwincode-client-port`

职责：提供 Server 与设备 Client 之间的 `ClientControlPort` 帧、游标和消息类型。

- 主要入口：`crates/winwincode-client-port/src/lib.rs`；协议位于 `src/domain.rs`、`src/exchange.rs` 和 `src/messages.rs`。
- normal 依赖：`winwincode-api`。
- 反向依赖：`device-client`、`control-plane`、`server`。
- 测试：没有独立 Cargo test target；单元测试随库目标运行。

## `winwincode-session`

职责：定义 `ProductSession` 生命周期、交互路由和执行会话绑定。

- 主要入口：`crates/winwincode-session/src/lib.rs::ProductSession`。
- normal 依赖：`winwincode-domain`。
- 反向依赖：`winwincode-control-plane`。
- 测试：`crates/winwincode-session/tests`，3 个 Cargo test target。

## `winwincode-delivery`

职责：定义 Delivery 领域、应用规则、投影和追加式 journal。

- 主要入口：`crates/winwincode-delivery/src/lib.rs`；`domain`、`application`、`projection` 和 `store` 是公开模块。
- normal 依赖：`winwincode-domain`。
- 反向依赖：`cli`、`storage`、`fusion`、`control-plane`、`server`、`test-assets`。
- 测试：`crates/winwincode-delivery/tests`，12 个 Cargo test target。

## `winwincode-publication`

职责：定义 Publication 领域、授权策略、外部操作和持久存储端口。

- 主要入口：`crates/winwincode-publication/src/lib.rs::PublicationCoordinator` 和 `PublicationPort`。
- normal 依赖：`winwincode-domain`。
- 反向依赖：`winwincode-storage`、`winwincode-control-plane`。
- 测试：`crates/winwincode-publication/tests`，2 个 Cargo test target。

## `winwincode-fusion`

职责：提供盲化独立推理面板、证据准入和 Provider 路由。

- 主要入口：`crates/winwincode-fusion/src/lib.rs`；核心公开函数是 `fusion_panel`。
- normal 依赖：`winwincode-delivery`。
- 反向依赖：`winwincode-codex`、`winwincode-control-plane`。
- 测试：`crates/winwincode-fusion/tests`，4 个 Cargo test target。

## `winwincode-storage`

职责：提供 Control Plane 的事务型 SQLite 产品存储。它保存状态、回执、outbox、调度、租约、设备绑定和 Publication。

- 主要入口：`crates/winwincode-storage/src/lib.rs::SqliteStorage` 和 `ProductStateStorage`。
- normal 依赖：`winwincode-delivery`、`winwincode-domain`、`winwincode-execution-port`、`winwincode-publication`。
- 反向依赖：`winwincode-cli`、`winwincode-control-plane`、`winwincode-server`。
- 测试：`crates/winwincode-storage/tests`，33 个 Cargo test target。

## `winwincode-audit`

职责：提供摘要链审计事件、审计存储、导出和保留策略。

- 主要入口：`crates/winwincode-audit/src/lib.rs`；实现位于 `src/event.rs`、`src/store.rs` 和 `src/export.rs`。
- normal 依赖：`winwincode-domain`。
- 反向依赖：`winwincode-control-plane`。
- 测试：`crates/winwincode-audit/tests`，4 个 Cargo test target。

## `winwincode-network`

职责：统一网络失败分类、重试判断、可取消请求和可选请求 journal。

- 主要入口：`crates/winwincode-network/src/lib.rs`；`NetworkFailure` 位于 `src/policy.rs`，可取消 I/O 位于 `src/transport.rs`。
- normal 依赖：无第一方 crate。
- 反向依赖：`device-client`、`provider`、`worker`、`control-plane`。
- 测试：`crates/winwincode-network/tests`，1 个 Cargo test target。

## `winwincode-repository-context`

职责：提供与基线绑定的仓库事实，并报告本地代码索引能力。

- 主要入口：`crates/winwincode-repository-context/src/lib.rs`。
- normal 依赖：无第一方 crate。
- 反向依赖：`winwincode-cli`、`winwincode-control-plane`。
- 测试：`crates/winwincode-repository-context/tests`，1 个 Cargo test target。

## `winwincode-observability-core`

职责：提供有界观测事实、聚合、告警和存储中立端口。

- 主要入口：`crates/winwincode-observability-core/src/lib.rs`。
- normal 依赖：无第一方 crate。
- 反向依赖：`local`。
- 测试：`crates/winwincode-observability-core/tests`，1 个 Cargo test target。

## `winwincode-provider`

职责：在设备侧保存 Provider 配置和密钥，并执行 HTTPS/SSE 模型请求、流转换、JEV 和扩展装配。

- 主要入口：`crates/winwincode-provider/src/lib.rs`、`src/device_store.rs::DeviceProviderStore` 和 `src/provider_https_sse.rs::HttpsSseProviderAdapter`。
- normal 依赖：`winwincode-api`、`winwincode-domain`、`winwincode-execution-port`、`winwincode-network`。
- 反向依赖：`device-client`、`worker`、`codex`、`control-plane`。
- 测试：`crates/winwincode-provider/tests`，4 个 Cargo test target；`src` 中还有单元和回归测试模块。

## `winwincode-kernel`

职责：封装 `third_party/codex`，并把 Codex 线程、回合、工具授权和模型流接入 WinWinCode。

- 主要入口：`crates/kernel/src/lib.rs`；模型桥位于 `crates/kernel/src/model_port.rs::KernelModelStreamTransport`。
- normal 依赖：`winwincode-execution-port`。它还依赖内嵌 Codex crate。
- 反向依赖：`winwincode-codex`。
- 测试：根目录 `tests/code_mode_kernel.rs`，1 个 Cargo test target；库内还有单元测试模块。

## `winwincode-kernel-helper`

职责：提供 Code Mode、沙箱和文件系统操作使用的内部辅助进程。

- 主要入口：`crates/helper/src/main.rs`。
- normal 依赖：无第一方 crate。它直接依赖内嵌 Codex 的 `codex-arg0` 和 `codex-code-mode-host`。
- 反向依赖：无工作区 normal 依赖。
- 测试：`crates/helper/tests`，1 个 Cargo test target。

## `winwincode-codex`

职责：把 Kernel 适配到 Worker。它管理 Codex 线程、模型桥、执行回执、候选产物和持久 outbox。

- 主要入口：`crates/winwincode-codex/src/lib.rs::ProductionCodexAdapter`；核心实现位于 `src/adapter.rs` 和 `src/model_bridge.rs`。
- normal 依赖：`winwincode-domain`、`winwincode-execution-port`、`winwincode-fusion`、`winwincode-kernel`、`winwincode-provider`。
- 反向依赖：`winwincode-worker`、`winwincode-server`。
- 测试：没有独立 Cargo test target；测试模块位于 `src`，跨 crate 纵向测试由 Worker 承载。

## `winwincode-worker`

职责：实现独立 Execution Worker 生命周期、工作区隔离、内嵌 Codex 执行和设备侧模型调用。

- 主要入口：`crates/winwincode-worker/src/main.rs` 和 `src/lib.rs::WorkerMain`；设备模型执行位于 `src/device_model.rs::DeviceModels`。
- normal 依赖：`winwincode-codex`、`winwincode-domain`、`winwincode-execution-port`、`winwincode-network`、`winwincode-provider`。
- 反向依赖：`winwincode-server`、`winwincode-local`。
- 测试：`crates/winwincode-worker/tests`，19 个 Cargo test target。

## `winwincode-control-plane`

职责：承载产品应用生命周期。它编排 Chat、Delivery、StrongFlow、Worker、Provider、Publication、审计和存储事务。

- 主要入口：`crates/winwincode-control-plane/src/lib.rs::ControlPlane` 和 `ControlPlaneInstanceRuntime`。
- normal 依赖：`api`、`audit`、`client-port`、`delivery`、`domain`、`execution-port`、`fusion`、`network`、`provider`、`publication`、`repository-context`、`session`、`storage`。
- 反向依赖：`winwincode-server`、`winwincode-local`。
- 测试：`crates/winwincode-control-plane/tests`，73 个 Cargo test target。

## `winwincode-local`

职责：在一个进程中组合 Control Plane 和 Worker，并用内存 ExecutionPort 转发类型化消息。

- 主要入口：`crates/winwincode-local/src/lib.rs::LocalLauncher` 和 `LocalExecutionPortHandle`。
- normal 依赖：`winwincode-control-plane`、`winwincode-observability-core`、`winwincode-worker`。
- 反向依赖：`winwincode-server`。
- 测试：`crates/winwincode-local/tests`，1 个 Cargo test target。

## `winwincode-server`

职责：提供独立 HTTP/WebSocket Server。当前生产入口装配 Control Plane、Device Client 通道和 Device 转发的 Worker 消息。库中保留受 `local-worker` feature 控制的同进程组合支持。

- 主要入口：`crates/winwincode-server/src/main.rs`、`src/server.rs::start_server_with_remote_worker` 和 `src/server.rs::router`。
- normal 依赖：`api`、`client-port`、`codex`、`control-plane`、`delivery`、`domain`、`execution-port`、`local`、`storage`、`worker`。其中 `codex`、`local` 和 `worker` 由 `local-worker` feature 控制。
- 反向依赖：`winwincode-cli`。
- 测试：`crates/winwincode-server/tests`，21 个 Cargo test target。

## `winwincode-device-client`

职责：实现设备身份、本地仓库映射、`ClientControlPort` 交换、占用状态、Worker 进程监管和 Worker 消息转发。

- 主要入口：`crates/winwincode-device-client/src/lib.rs`；守护循环位于 `src/daemon.rs`，SQLite 存储位于 `src/store.rs::DeviceStore`。
- normal 依赖：`winwincode-api`、`winwincode-client-port`、`winwincode-network`、`winwincode-provider`。
- 反向依赖：`winwincode-cli`。
- 测试：`crates/winwincode-device-client/tests`，13 个 Cargo test target。

## `winwincode-evidence-export`

职责：为本地 Delivery 生成确定性的离线证据包。

- 主要入口：`crates/winwincode-evidence-export/src/lib.rs`。
- normal 依赖：无第一方 crate。
- 反向依赖：`winwincode-cli`。
- 测试：`crates/winwincode-evidence-export/tests`，1 个 Cargo test target。

## `winwincode-test-assets`

职责：分析确定性测试资产及其操纵风险。

- 主要入口：`crates/winwincode-test-assets/src/lib.rs`。
- normal 依赖：`winwincode-delivery`、`winwincode-domain`。
- 反向依赖：`winwincode-cli`。
- 测试：`crates/winwincode-test-assets/tests`，2 个 Cargo test target。

## `winwincode-cli`

职责：提供 `wwc`。命令覆盖仓库接入、Device 服务与管理、诊断和备份。

- 主要入口：`crates/winwincode-cli/src/main.rs` 和 `src/lib.rs::run_cli`。
- normal 依赖：`delivery`、`device-client`、`domain`、`evidence-export`、`repository-context`、`server`、`storage`、`test-assets`。
- 反向依赖：无工作区 normal 依赖。
- 测试：`crates/winwincode-cli/tests`，4 个 Cargo test target。

## 最大的第一方 Rust 源文件

下表按物理行数排序。范围只包含 `crates/**/*.rs`。它包含生成文件和测试文件。

| 排名 | 路径 | 行数 |
| ---: | --- | ---: |
| 1 | `crates/winwincode-codex/src/adapter.rs` | 13,687 |
| 2 | `crates/winwincode-api/src/generated.rs` | 10,836 |
| 3 | `crates/winwincode-execution-port/src/generated.rs` | 8,509 |
| 4 | `crates/winwincode-worker/tests/worker_lifecycle.rs` | 8,297 |
| 5 | `crates/winwincode-worker/tests/production_vertical.rs` | 7,167 |
| 6 | `crates/winwincode-codex/src/model_bridge.rs` | 6,333 |
| 7 | `crates/winwincode-control-plane/tests/product_session_execution_application.rs` | 5,832 |
| 8 | `crates/winwincode-codex/src/store.rs` | 5,620 |
| 9 | `crates/winwincode-worker/src/lib.rs` | 5,224 |
| 10 | `crates/winwincode-provider/src/provider_https_sse.rs` | 5,114 |

生成文件用于保持 Rust、TypeScript 和协议契约一致。大型手写实现集中在 Codex 适配、Worker 生命周期和 Provider SSE 路径。相关变更应优先依赖 [测试策略](../testing.md) 中的纵向测试和契约测试。
