# ADR-0028 附录：目标模块图与依赖门禁

- 正式决定：[ADR-0028](0028-control-plane-worker-migration.md)
- 机器可读目标图：[0028-control-plane-worker-target-graph.json](0028-control-plane-worker-target-graph.json)
- 当前迁移清单：[0028-control-plane-worker-migration.inventory.json](0028-control-plane-worker-migration.inventory.json)

## 这份文件是什么

这是目标声明，不是完成声明。机器可读目标图说明迁移结束后每个模块应当负责什么、可以
引用哪些产品模块，以及当前清单中的源码在哪个阶段迁移。目标 crate 尚未建立时，检查
只验证这份计划内部一致，并不把计划中的目录当成已经实现。

当某个目标 crate 的 `Cargo.toml` 出现后，同一项检查会通过 `cargo metadata` 读取实际
Rust workspace。如果实际引用超出目标图的许可范围，检查立即失败。因此，文件既能在
阶段 1 冻结方向，也能在阶段 2 到阶段 6 持续阻止语言翻译把旧职责带进错误模块。

完成状态仍以 Beads 阶段任务、行为基线和发布门禁为准，不能由目标图中的节点名称推断。

## 目标调用方向

```text
TypeScript Web
  ├─ HTTP command/query ───────┐
  └─ WebSocket projection ─────┤
                               ↓
                    Rust Control Plane
                      ├─ Provider Gateway ── Credential Vault
                      └─ ExecutionPort
                               ↓
                    Rust Execution Worker
                               ↓
                       Embedded Codex Core
```

主要业务写入由 Control Plane 接收。Worker 只接收经过 `ExecutionPort` 分配的工作，
Codex Core 只接收 Worker 适配后的执行请求。任何本地同进程优化也要经过相同接口。

## 必须长期成立的五条规则

### 1. Control Plane 不得依赖 Codex Core

`winwincode-control-plane` 及其能到达的所有产品 crate 都不得引用
`winwincode-codex` 或 `codex-*` 包。Control Plane 可以引用
`winwincode-execution-port`，但不能通过公共 crate 间接带入 Codex。

这条规则保证组织、交付、审批、模型治理和持久化可以脱离代码执行进程部署。

### 2. Worker 不得依赖产品业务模块

`winwincode-worker` 可以引用共享 ID、事件包络、`ExecutionPort`、可观测性和
`winwincode-codex`。它不得引用 Identity、DeliveryStore、Collaboration、Publication、
ProductSession、Approval、Credential、Provider 或 Control Plane 组合模块。

Worker 只上报事实和产物引用。Control Plane 校验租约与 fencing token 后，才把这些事实
写入产品状态。

### 3. Web 只能访问 Control Plane

`apps/web` 只能通过生成客户端使用 `control-plane-http` 和
`control-plane-websocket`。页面与手写客户端不能直接创建 `fetch` 或 `WebSocket`，也
不能引用 `ExecutionPort` 或 Worker 包。实际网络实现集中在
`apps/web/src/generated`，这样浏览器没有第二条写入或执行路径。

### 4. 本地启动器只负责组装

`winwincode-local` 只负责读取进程配置、组装 Control Plane、组装本地 Worker，以及
启动和停止进程。它只能直接引用三个产品 crate：

- `winwincode-control-plane`
- `winwincode-worker`
- `winwincode-observability`

业务状态机、Provider、凭据、工作区和 Codex 适配不放进启动器。本地版与企业版的区别
只是两个模块是否在同一进程，不是另建一套业务实现。

### 5. Provider Gateway 是长期模型凭据的唯一使用者

`winwincode-provider` 是中央 Provider Gateway，`winwincode-credential` 负责从
Keychain、Vault 或 KMS 解析长期凭据。Worker 只通过
`execution-port-model-stream` 提交模型请求并接收流，既不引用 Provider/Credential
crate，也不保存长期密钥。

## 目标模块分区

机器可读文件中的 `zone` 表示模块所在区域：

| 区域 | 作用 | 主要节点 |
| --- | --- | --- |
| `presentation` | 页面与生成客户端 | `typescript-web`、`typescript-generated-client` |
| `contract` | 唯一公共 schema | `canonical-schema` |
| `shared` | 双方都可使用且不拥有业务的窄类型 | Domain ID、Events、ExecutionPort、Observability |
| `control-plane` | 产品状态与外部治理 | Delivery、Session、Provider、Publication、Identity 等 |
| `execution-worker` | 工作区与执行协调 | Worker、Codex Adapter |
| `composition` | 本地同进程启动 | `winwincode-local` |

`allowedInternalDependencies` 是产品内部引用的允许清单，不是建议清单。新增产品 crate 时，
必须先把职责、区域和允许引用写入目标图；没有声明的 `winwincode-*` 引用会使检查失败。

## 迁移阶段覆盖

`migrationPhases` 对迁移清单中的每个 surface 做一次且仅一次映射：

| 阶段 | 当前源码范围 |
| --- | --- |
| 1 | canonical schema 与生成合同 |
| 2 | Delivery/StrongFlow 和 Control Plane 存储 |
| 3 | Publication、GitHub、Audit 与 Policy |
| 4 | ProductSession、Scheduler、Worker、工作区和 Codex Adapter |
| 5 | Provider、Model 与 Credential |
| 6 | TypeScript Web、CLI/本地启动、DSH/Cordis/N-API 删除 |

静态检查会把 `migration.inventory.json` 的 surface ID、阶段和 `targetModules` 与目标图逐项
比对。清单增加源码区域但没有目标节点或阶段时，检查失败。企业新增的 Identity、Project
和 Collaboration 节点使用 `enterprise` 标记，因为它们由后续企业协作 Epic 实现，不会
伪装成阶段 1 已交付内容。

## 当前检查与未来检查

`tests/control-plane-worker-dependency-contract.test.mjs` 执行两层验证：

1. 始终验证目标图内部一致、五条规则成立、迁移 surface 全覆盖；
2. 目标 crate 存在后，读取 `cargo metadata`，验证它已加入 workspace，且实际产品依赖
   没有超出 `allowedInternalDependencies`。

Web 目录出现后，检查还会拒绝生成目录之外的直接网络调用和 Worker/ExecutionPort
引用。本地启动器出现后，检查会要求它只引用两个运行模块和公共可观测性模块。

这些检查只证明模块引用方向正确。HTTP、WebSocket、ExecutionPort、生成类型、行为差异
和四平台产物仍由各自阶段的合同测试与发布门禁证明。
