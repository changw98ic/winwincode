# ADR-0033：Community Engineering Runtime 权威边界与迁移

- 状态：Accepted
- 日期：2026-09-08
- 对应任务：`WWC-ER-0002` / `winwincode-hrw`
- 前置审计：`WWC-ER-0001` / `winwincode-dwz`；审计快照 [`backlog-migration.json`](../engineering-runtime/backlog-migration.json)，源计划 SHA-256 `4359a7084e904640a1e4383d70278101cbbbf9aabf2eb4afc2ba78ff834671f8`（以仓库记录为准）
- 上层运行边界：[ADR-0028](0028-control-plane-worker-migration.md)
- Client 设备、占用与 fencing 历史边界：[ADR-0030](0030-multi-user-client-access-and-occupancy.md)；身份受众以 [ADR-0031](0031-three-product-editions.md) 为准
- 持久化边界：[0032-community-persistence-ports.md](0032-community-persistence-ports.md)

> 本 ADR 根据本轮用户授权实施，并采用 root 作为实现负责人的默认裁决；不表示全部 E01–E13 业务已完成。

## 背景与证据

现有 ADR-0028 已将 Control Plane 定义为产品状态唯一写入方、Worker 定义为一次执行协调方、Kernel 定义为 Codex 执行事实权威。ADR-0030 中的 Client 设备、占用与 fencing 分层继续有效，身份和受众边界由 ADR-0031 的 Community 单 Owner 决定取代。dwz 审计覆盖 111 条 active 旧任务，分类为 KEEP 59、REWRITE 47、MERGE 2、DEFER 3；证据级别和 CI 事实仍按审计记录保留，不把映射或历史关闭记录误报为全产品实现验收。

## 决定

### 1. 唯一权威与角色

| 角色 | 唯一职责与权威 | 明确禁止 |
|---|---|---|
| **Controller（Control Plane）** | 唯一写入并改变 `WorkItem`、Attention、完成/失败/取消/rework 状态；校验权限、依赖、Contract revision、Attempt 身份；原子写领域状态与 outbox | Worker、Verifier、Planner、Human 直接写 WorkItem 终态或绕过命令/事务 |
| **Worker** | 在 Controller 授权的 `WorkItem`/`Attempt`/Lease/Fencing 下执行；提交 Candidate、执行事实、请求和候选结果 | 宣称 WorkItem done、改变产品状态、创建第二租约/执行器/数据库 |
| **Verifier** | 只读指定 Candidate、Contract revision 与 `VerificationPlan`；产生 Evidence/VerificationResult/Verdict；不得修改产品代码或 Candidate | 直接改代码、移动 Git ref、写 WorkItem 状态、用模型自述代替 machine evidence |
| **Planner** | Worker 内部能力：把 Contract/WorkItem 拆成动态 plan items，供执行、UI、handoff、恢复使用 | 顶级角色、独立状态机、完成权威、独立调度队列 |
| **Human** | Authority Endpoint：仅处理机器无法代行的授权/确认/解锁/裁决，并以 Controller command 留痕 | 第四执行引擎、直接编辑数据库或绕过验证 |
| **Codex Core** | 继续拥有 Thread、Turn、工具、Shell、沙箱、Diff、用量及执行恢复事实；嵌入路径不变 | Engineering Runtime 复制或替换 Codex 执行器 |

Controller 的状态投影是唯一产品状态真源；Worker/Verifier 的本地视图和运行日志均为候选事实，必须带身份、版本和 fencing 校验后提交。

### 2. 对象关系（多 WorkItem、多 WorkRun）

```text
WorkContract(revision)
  └─< WorkItem
       └─< WorkRun ──1 Attempt ──1 new ExecutionLease
                    └─1 WorkerSession ──1 CodexThread
                         └─0..1 accepted Candidate
                              └─< VerificationPlan ──< VerificationResult/Evidence/Verdict

WorkItem 依赖 WorkItem（DAG）。多个 WorkRun 可复用同一已确认 commit；每个 WorkRun 独立授权与记录。
```

- `WorkContract` 是不可变 revision；改变目标、验收、约束、protected scope 或所需人工授权必须产生新 revision。Candidate 永远绑定产生它的 Contract revision。WorkContract、WorkItem、WorkRun、VerificationPlan 使用现有 canonical ULID 标识规范；不得另造 ID 编码。
- `WorkItem` 是独立可调度单元，可并行存在多个 active item；它引用 Contract revision 与 dependency DAG，不再由全局 `Plan → Execute → Verify` 三阶段包住。
- 一个 `WorkItem` 可有多个 `WorkRun`；每个 `WorkRun` 绑定一次 `Attempt`、一个新的 `ExecutionLease`、一个 WorkerSession/CodexThread，并最多产生一个被 Controller 接受的 Candidate。
- `Attempt` 是运行重试身份。retry/replacement 新建 WorkRun/Attempt 与新的 ExecutionLease；可保留 AgentIdentity、WorkItem、Profile、Workspace 逻辑绑定。恢复可在安全校验下继续同一 CodexThread/逻辑线程；禁止的是旧 lease/fencing 写入，不是安全的 same-thread resume。安全条件至少包括同一 WorkItem/Contract revision、同一逻辑 AgentIdentity、同一 Workspace/source revision、当前 lease/fencing 与 SessionBinding 匹配，并由 Controller 原子确认恢复。
- 多个 WorkRun 可复用同一已确认 commit，但 Candidate 绑定与验证独立；这不是通用多对多 Candidate 模型；后续扩展必须另行更新 ADR 与 canonical schema。
- `VerificationPlan` 是对 Candidate + Contract revision + acceptance criteria 的只读验证声明；可有多个 verifier/plan 并行。Verifier 的 Verdict 是提交给 Controller 的事实，不是 WorkItem 状态命令；Verdict 持久化和状态消费仍由 Controller 完成。

### 3. 状态与命令

WorkItem 的规范状态为：`backlog → ready → in_progress → waiting_dependency | waiting_human | candidate_ready | validating → done | rework | failed | cancelled`。所有转移由 Controller 根据已持久化事实、依赖、验证结果和 Human command 决定。Worker 只能提交 `execution_intent`、Candidate 和事实；Verifier 只能提交 Evidence/VerificationResult/Verdict；Planner 只能更新内部 plan projection。

`done` 需要 Controller 看到满足 Contract revision 的必要 Evidence/Verdict。Verifier 只报告结果事实，Controller 校验并持久化 Verdict 后决定最终状态。`inconclusive` 表示证据不足，默认进入 `rework`；`infra_error`/执行环境错误按分类进入重试、`failed` 或 `waiting_dependency`。普通机器失败不会仅因重试耗尽自动变成 Human 授权问题；只有确需授权的分类才进入 `waiting_human`。任何重试、恢复、重复提交都按幂等 receipt 处理。

### 4. 事务与恢复边界

一次 Controller command 在一个存储事务内：校验 expected revision/身份/lease → 写 WorkItem/Attempt/Candidate/Verification 或 Attention 领域记录 → 写 outbox → 提交；外部 Worker、Git、Codex、Verifier 进程不在该事务内。outbox 发布可重试，消费按 receipt 去重。

Worker/Codex 执行事实先写其既有本地运行时存储，再向 Controller 提交带 WorkRun、Attempt、WorkerSession、CodexThread、ExecutionLease/Fencing 的结果。ExecutionLease 的权威是 `crates/winwincode-storage/src/execution_registry.rs` 的 durable `ExecutionLeaseRecord`/`ExecutionDispatchAuthority`/terminal request；不引入 temporary-root lease 作为执行租约。Verifier 只读 Candidate/Git ref 与冻结输入，向 Controller 提交 Evidence/VerificationResult/Verdict；Controller 原子持久化并消费结果推进状态。进程崩溃恢复依赖既有 SessionBinding、ExecutionRegistry lease/fencing、SQLite/outbox 和 receipt-first replay，不新增 recovery 数据库。旧 lease/fencing 的迟到写入拒绝；安全恢复可继续同一逻辑线程，replacement 则使用新 WorkRun/Attempt/lease，并保留 AgentIdentity、WorkItem、Profile、Workspace 绑定。

### 5. 直接切换与旧任务唯一去向

当前版本仍是首个 alpha 候选，没有承诺支持的旧本地数据库升级合同。`WWC-ER-0003` 因此不发布旧 `StageRun` 到 `WorkRun` 的转换程序、迁移数据库或回执表；开发期旧数据和夹具直接按当前结构重建。`WWC-ER-0005`、`0006` 负责把 UI/API 一次切到唯一新路径并删除旧写入，不保留旧产品兼容副本或第二套状态机。旧 `StageRun` 输入不能生成可派发 `WorkRun`，新写入不依赖全局 StageRun/Plan/Execute/Verify。

审计映射保持旧任务唯一 owner；KEEP 任务复用现有 Candidate、Lease/Fencing、SQLite/outbox、SessionBinding 和 Git ref 能力，只补新身份绑定；REWRITE 任务待本 ADR 与合同冻结后逐项创建最小差异；MERGE 任务合并到已有 owner；DEFER 不提前实现。不得凭 ADR 文本宣布任务完成。

ReAct、DebugProbe、DelegatedBatch 是 Worker 内部执行模式，不等同旧顶层三阶段，不因迁移而删除。

### 6. 不新增基础设施

复用既有嵌入 Codex Core、WorkerSession/Job/Lease/Fencing、Candidate/Git ref、SessionBinding、SQLite 事务/outbox、ExecutionPort 与 Control Plane 命令接口；不另造执行器、租约系统、数据库、全局 Plan 状态或兼容产品路径。Cloud/Enterprise 未验收的迁出源码仍按各自迁移门禁处理，Community ADR 不提前删除。

## 影响与已采用裁决

收益是单一状态权威、可并行多 WorkItem/多 WorkRun/Candidate/VerificationPlan，且执行与验证可独立失败、重放和恢复。代价是迁移需补齐身份绑定、幂等和跨模块合同门禁；本 ADR 不把后续业务接线误报为完成。

本轮用户已授权推进，root 作为实现负责人采用以下默认裁决：

1. 一个 WorkItem 可有多个 WorkRun；每个 WorkRun 一次 Attempt、一个新 ExecutionLease、最多一个 accepted Candidate；同 commit 跨 run 复用但验证绑定独立。
2. `inconclusive` 进入 `rework`；`infra_error` 按既有错误分类进入 retry、`failed` 或 `waiting_dependency`；普通机器失败不自动转 Human 授权。
3. 恢复窗口沿用现有配置和 ADR-0030 `recovery_pending`/安全清理；same-thread resume 必须满足本文身份、revision、workspace、lease/fencing 和 SessionBinding 条件；replacement 使用新 Attempt/lease 并保留逻辑身份。
4. 旧 `StageRun` 数据不进入当前运行存储，也不能生成 WorkItem 或 WorkRun。

## E00 交付边界与后续验收

本 ADR 收口 E00 四项交付边界：

- `WWC-ER-0001`：审计门——111 条旧任务具有唯一 KEEP/REWRITE/MERGE/DEFER 去向；映射与 Beads/CI 门禁由审计任务维护。
- `WWC-ER-0002`：职责决定——本 ADR 冻结 Controller/Worker/Verifier/Planner/Human 权威及 WorkContract→WorkItem→WorkRun→Candidate→VerificationPlan→Evidence/Verdict 关系。
- `WWC-ER-0003`：直接切换决定——首个 alpha 不维护旧数据库转换程序；删除迁移专用代码、表和测试，开发数据按当前 WorkRun 结构重建。
- `WWC-ER-0004`：规范 schema 与生成一致性——按本 ADR 的字段、ID、关系、状态和拒绝规则更新 canonical schema，并通过 generator 生成 Rust/TypeScript/OpenAPI；不在本 ADR 中直接修改 schema 或生成物。

E01–E13 的业务接线、生产实现和纵向验收继续由既有 Beads 任务承接；本 ADR 接受不等于全产品完成。

接受后的实现门包括：Controller-only 状态写入、WorkItem 多 WorkRun、WorkRun 单 Attempt/lease/accepted Candidate、独立 VerificationPlan、duplicate/stale identity/restart/rework/recovery 测试，以及 schema/generator drift 检查。
