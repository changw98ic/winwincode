# ADR-0028 附录：单一路径模块图与依赖门禁

- 正式决定：[ADR-0028](0028-control-plane-worker-migration.md)
- 机器可读目标图：[0028-control-plane-worker-target-graph.json](0028-control-plane-worker-target-graph.json)
- 机器可读源码清单：[0028-control-plane-worker-migration.inventory.json](0028-control-plane-worker-migration.inventory.json)
- 门禁测试：[`tests/control-plane-worker-dependency-contract.test.mjs`](../../tests/control-plane-worker-dependency-contract.test.mjs)

## 这份文件是什么

这是已接受的单一路径合同。目标图中的每个目录都必须存在，`path`、Rust package name、职责和 `allowedInternalDependencies` 必须对应当前源码。目标图不保留缺失的计划模块，也不为旧入口或临时适配器留下允许边。

Rust 依赖门禁使用 `cargo metadata --locked` 读取已存在的 workspace package。`crates/*/Cargo.toml` 与目标图中的 Rust 节点必须双向完全一致，不能出现只在 workspace 或只在目标图中的 package。生产依赖必须在节点的精确允许清单中；测试专用依赖单独检查，不会扩大生产闭包。目录、模块和公共合同发生变化时，要同时更新目标图、源码清单和测试。

## 唯一调用方向

```text
apps/client
  ├─ generated HTTP command/query ──┐
  └─ generated WebSocket projection ─┤
                                     ▼
                              winwincode-server
                                     │
                                     ▼
                         winwincode-control-plane
                                     │ ExecutionPort
                                     ▼
                            winwincode-worker
                              ├─ winwincode-codex
                              │    └─ winwincode-kernel-helper
                              └─ winwincode-kernel

winwincode-local ── 只组装 Control Plane 与 Worker
```

Server 是唯一公开网络边界，Control Plane 是唯一产品状态写入方，Worker 是唯一工作区和执行协调方，Kernel 是唯一 Codex 执行事实方。

## 五条长期规则

### 1. Control Plane 不得到达 Codex 执行模块

`winwincode-control-plane` 及其生产依赖可以使用 `winwincode-domain`、`winwincode-api`、
`winwincode-execution-port`、`winwincode-storage`、`winwincode-delivery`、
`winwincode-session`、`winwincode-publication`、`winwincode-audit` 和
`winwincode-repository-context`。它们不能到达 `winwincode-codex`、`winwincode-kernel` 或 Codex 上游 crate。

这保证产品状态、策略、收据和审计可以独立于执行事实部署。Repository Context 只返回明确 Git commit 上的基线、文件清单和索引能力，不读取可变工作区、不执行 Job、不写产品状态。

### 2. Worker 只持有执行闭包

`winwincode-worker` 的生产依赖精确为：

- `winwincode-codex`；
- `winwincode-domain`；
- `winwincode-execution-port`。

Worker 负责 WorkerSession、Job、Lease、Fencing、工作区、产物、运行事件、取消和结果；它不依赖产品持久化、Delivery、Session、Publication、Audit 或 Control Plane。Worker 通过 ExecutionPort 上报带身份事实，Control Plane 校验后才持久化。

### 3. Client 只能访问 Server

`apps/client` 的页面只依赖 `apps/client/src/generated` 和一个 `serverUrl` facade。生成 Client 是 HTTP Command、Query 和 WebSocket 的唯一网络实现；页面不直接创建传输连接，不引用 Worker、ExecutionPort 或 Rust 业务 crate。

pnpm workspace 精确包含 `apps/client`、`packages/contracts` 和 `packages/strongflow`。`packages/strongflow` 只依赖 `packages/contracts`，保存 Delivery domain/projection 合同，不持有网络或运行时权限；三者和锁文件 importer 必须双向一致。

### 4. Local 只负责组装

`winwincode-local` 的生产依赖精确为 `winwincode-control-plane`、`winwincode-worker` 和 `winwincode-observability`。它读取配置、创建数据根、连接 typed frame、启动和停止模块；产品状态、Provider、Credential、工作区和 Kernel 责任留在各自所有者。

### 5. Server 与 Helper 的边界固定

`winwincode-server` 只组合公开网络边界所需的 `winwincode-api`、Control Plane、Worker、Codex、Local、Storage、Domain 和 ExecutionPort。Server 不把业务写入复制到路由层。

`winwincode-kernel-helper` 是独立的 Rust 可执行文件。`winwincode-codex` 在启动 Kernel 前校验 Helper 的来源摘要、版本、签名、大小和握手身份；Helper 不提供 HTTP、WebSocket 或产品状态接口。

## Provider 与 Credential

Provider Gateway 和 Credential 解析属于 `winwincode-control-plane` 内部模块。长期凭据的唯一使用者是 Control Plane；Worker 通过 `execution-port-model-stream` 获取受策略约束的模型流，消息只携带短期引用和结果事实。公开合同、运行事件、审计和 Delivery 不保存原始 secret。

## 模块区域

| 区域 | 责任 | 节点 |
| --- | --- | --- |
| `presentation` | 页面、生成客户端与 StrongFlow 投影合同 | `typescript-web`、`typescript-generated-client`、`typescript-strongflow` |
| `contract` | canonical schema 与共享 TypeScript 合同 | `canonical-schema`、`typescript-contracts` |
| `shared` | 不拥有业务状态的窄类型与基础设施 | `winwincode-domain`、`winwincode-api`、`winwincode-execution-port`、`winwincode-storage`、`winwincode-observability` |
| `control-plane` | 产品状态、策略、仓库事实和外部治理 | `winwincode-control-plane`、`winwincode-delivery`、`winwincode-session`、`winwincode-publication`、`winwincode-audit`、`winwincode-repository-context` |
| `execution-worker` | 工作区、执行协调、Kernel 和 Helper | `winwincode-worker`、`winwincode-codex`、`winwincode-kernel`、`winwincode-kernel-helper` |
| `composition` | HTTP 边界、本机组装和运维入口 | `winwincode-server`、`winwincode-local`、`winwincode-cli`、`winwincode-drill` |
| `control-plane` 的 enterprise/support 节点 | 备份、证据、连接器和存储 adapter | `winwincode-backup`、`winwincode-evidence-export`、`winwincode-integration`、`winwincode-object-store`、`winwincode-postgres`、`winwincode-test-assets` |

`allowedInternalDependencies` 是精确集合。每个依赖都必须同时在目标图中出现；未知节点、未声明边和路径重复均使门禁失败。

## 当前检查

测试分成六层：

1. 验证图状态、节点 ID、路径、Rust/npm package name、职责和允许边；
2. 验证 Client、Server、Worker、Local、Kernel、Helper 的边界与接口消费者；
3. 验证源码清单的 source root、surface、phase、target module 和行为合同；
4. 读取 `cargo metadata --locked`，双向核对全部 25 个 workspace crate 与目标图节点，并核对每个生产依赖；同时核对 pnpm 的 3 个 workspace package、锁 importer 与内部 package 依赖；
5. 扫描 `apps/client`，确保网络调用集中在 generated Client；
6. 检查文档中的相对链接、生成合同和格式。

本地建议执行：

```bash
node --test tests/control-plane-worker-dependency-contract.test.mjs
node --test tests/control-plane-worker-inventory.test.mjs
corepack pnpm contracts:check
corepack pnpm format:check
```

完整发布状态还要结合 [release gate](../release-gate.md) 和 Beads 任务结果判断；图文件只声明模块边界，不替代运行门禁。
