# ADR-0023：WinWinCode 只拥有交付目标、交付状态和交付结论

- 状态：已接受
- 日期：2026-08-23
- 演进说明：十对象 Delivery 模型和 Codex 唯一执行权威保持有效；DSH 后端职责正按 [ADR-0028](0028-control-plane-worker-migration.md) 迁入 Rust Control Plane 与 Rust Execution Worker。
- 对应任务：`winwincode-9c4.13.1`、`winwincode-9c4.13.2`、`winwincode-9c4.13.3`、`winwincode-9c4.7.1`、`winwincode-9c4.7.2`、`winwincode-9c4.7.3`、`winwincode-9c4.7.4`、`winwincode-9c4.8.1`、`winwincode-9c4.8.2`、`winwincode-9c4.8.3`、`winwincode-9c4.8.4`、`winwincode-9c4.9.6`、`winwincode-9c4.10.1`、`winwincode-9c4.10.2`、`winwincode-9c4.10.3`、`winwincode-9c4.11.1`、`winwincode-9c4.11.2`
- 合同实现：[`packages/contracts/src/delivery.ts`](../../packages/contracts/src/delivery.ts)
- 调用格式：[`packages/contracts/src/strongflow-delivery-api.ts`](../../packages/contracts/src/strongflow-delivery-api.ts)
- 服务与存储：[`packages/strongflow/src/delivery-service.ts`](../../packages/strongflow/src/delivery-service.ts)、[`packages/strongflow/src/delivery-store.ts`](../../packages/strongflow/src/delivery-store.ts)
- 执行语义投影：[`packages/dsh-profile/src/runtime-events.ts`](../../packages/dsh-profile/src/runtime-events.ts)、[`packages/strongflow/src/delivery-runtime-projection.ts`](../../packages/strongflow/src/delivery-runtime-projection.ts)
- 图上执行投影：[`packages/contracts/src/strongflow-diagram-execution.ts`](../../packages/contracts/src/strongflow-diagram-execution.ts)、[`packages/strongflow/src/diagram-execution-projection.ts`](../../packages/strongflow/src/diagram-execution-projection.ts)
- GitHub 发布绑定：[`packages/contracts/src/strongflow-github-publication.ts`](../../packages/contracts/src/strongflow-github-publication.ts)、[`packages/strongflow/src/github-publication.ts`](../../packages/strongflow/src/github-publication.ts)
- GitHub 审核包：[`packages/contracts/src/strongflow-github-review-package.ts`](../../packages/contracts/src/strongflow-github-review-package.ts)、[`packages/strongflow/src/github-review-package.ts`](../../packages/strongflow/src/github-review-package.ts)
- GitHub 副作用控制：[`packages/strongflow/src/github-publication-provider.ts`](../../packages/strongflow/src/github-publication-provider.ts)、[`packages/strongflow/src/github-publication-journal.ts`](../../packages/strongflow/src/github-publication-journal.ts)、[`packages/strongflow/src/github-publication-runner.ts`](../../packages/strongflow/src/github-publication-runner.ts)
- 重启协调：[`packages/dsh-profile/src/delivery-recovery.ts`](../../packages/dsh-profile/src/delivery-recovery.ts)
- 独立验证投影：[`packages/strongflow/src/independent-verification.ts`](../../packages/strongflow/src/independent-verification.ts)
- DSH 与 CLI 入口：[`packages/strongflow/src/delivery-remote.ts`](../../packages/strongflow/src/delivery-remote.ts)、[`apps/host/src/strongflow-cli.ts`](../../apps/host/src/strongflow-cli.ts)
- 确定性验收基座：[ADR-0024](0024-deterministic-delivery-fixture.md)

## 结论

WinWinCode 是 Codex Core 之上的交付控制层，不是第二套 Agent 内核，也不是通用任务、权限、会话或监控平台。

```text
TypeScript Web UI
负责聊天与 StrongFlow 表现、表单、图表和 HTTP/WebSocket 客户端

Rust Control Plane
负责 ProductSession、Provider Gateway、凭据引用、Delivery、审批和持久化

Rust Execution Worker
负责 ExecutionJob、WorkerSession、工作区和 Codex Core 适配

Codex Core
负责 CodexThread、Plan、Agent Graph、工具、Shell、MCP、沙箱、权限和执行恢复
```

各层通过强类型标识和只读投影关联。一个事实只能有一个所有者。其他层可以保存引用或生成界面视图，不复制一份可独立修改的权威状态。

## 所有权清单

| 事实 | 唯一所有者 | WinWinCode 的处理 |
| --- | --- | --- |
| Codex Thread、Turn、上下文和历史 | Codex Core | 在 `SessionBinding` 保存 `CodexThreadId` 引用 |
| Plan 与 Plan Item | Codex Core | 保留结构化事件并投影进度，不调度 |
| Agent Graph、子 Agent 生命周期和通信 | Codex Core | 展示原图，不建立 roster、mailbox 或第二张图 |
| Tool、Shell、MCP、Sandbox、Permission | Codex Core | 展示和引用执行事实，不实现第二套运行时 |
| 执行审批与危险操作判断 | Codex Core | Control Plane 只投影并记录审批结果引用 |
| Chat 与产品会话 | Rust Control Plane | 保存 `ProductSessionId`，UI 只读写 API Projection |
| Provider、Model、Credential | Rust Control Plane | Provider Gateway 解析凭据引用，Worker 不持有长期密钥 |
| ExecutionJob、租约和 Worker 运行 | Rust Control Plane / Rust Execution Worker | 保存 `ExecutionJobId`、`WorkerSessionId`、lease 与 fencing 事实 |
| Delivery 目标与验收条件 | WinWinCode | 保存 `DeliverySpec` 与 `AcceptanceCriterion` |
| 跨 Session 的交付阶段 | WinWinCode | 保存 `StageRun` 与 `SessionBinding` |
| 业务问题与人工决定 | WinWinCode | 保存 `AttentionItem` 及其解决结果 |
| 验收依据与结论 | WinWinCode | 保存 `EvidenceRef`、`CriterionResult` 和 `DeliveryVerdict` |

旧 DSH Agent Teams 不进入目标运行合同。执行中的子 Agent 属于 Codex Core；把它们再接到一个 StrongFlow 任务图会产生两份可以互相冲突的执行状态。

## 唯一的交付数据模型

第一版只有十个顶层业务对象：

```text
Delivery
├── DeliverySpec
│   └── AcceptanceCriterion[]
├── DeliveryTask[]
├── StageRun[]
│   └── SessionBinding[]
├── AttentionItem[]
├── EvidenceRef[]
└── DeliveryVerdict
    └── CriterionResult[]
```

### DeliverySpec

`DeliverySpec` 保存标题、目标、范围、明确排除项、约束、仓库、基线版本、稳定的验收条件和当前定义允许的返工次数。它回答“到底交付什么”“什么算完成”和“同一份批准定义最多允许几次代码返工”。至少要有一个必需验收条件；仓库和基线不能为空；`maxReworkAttempts` 为零到 100 的整数。

来自 GitHub Issue 的 Delivery 另外保存一个最小 `sourceRef`：提供商、仓库和 Issue 编号。所有 Delivery 身份统一使用 `dlv_` 加 26 位 Crockford Base32 字符。GitHub 来源先把仓库名转成小写，再对版本化命名空间、提供商、类型、仓库和 Issue 编号的规范字节计算 SHA-256，取前 128 位生成该身份；因此同一个 Issue 在 TypeScript 和 Rust 中总是解析到同一个 Delivery。旧的 `github-issue:owner/repository:number` 形式不是 Delivery ID，并在创建和读取时直接拒绝。没有外部来源的 Delivery 才生成新的 ULID。可选的 `publicationTarget` 只保存一个 Pull Request 目标所需的 base/head 仓库和分支。Spec revision 可以修改交付内容，但来源和目标在同一个 Delivery 生命周期中保持稳定；GitHub 的标题、正文、状态、标签、负责人、评论和项目字段不进入 DeliverySpec。

需求与方案严格分开：`DeliverySpec` 是交付目标、范围、排除项、约束和验收条件；结构化方案、系统架构图、流程图、风险和未决事项由规划阶段产生并供人工审核。方案不是另一个任务系统，也不能修改已批准的验收条件。

规划结束时，服务生成 `winwincode.plan-review-context.v2` 协议片段，并把它保存在一个阻塞 `AttentionItem.context` 中。片段绑定当前 Delivery、Spec ID/revision、规划 `StageRun`、规划 `SessionBinding`、人工审核 `StageRun` 和 Attention ID；同时冻结结构化方案、两张图、每个方案组件负责的仓库相对路径前缀、风险、未决事项和整个审核集合的 SHA-256。图由服务根据平台固定边界和结构化方案确定性生成，审核时会重新生成并逐项比对，因此调用方不能替换节点、关系或流程后继续使用旧摘要。该片段属于现有 Attention，不是第十一个顶层业务对象。

当前 `DeliverySpec` 通过人工 Plan Review 后，验证器从 Delivery 生成一份只读的验收输入。它逐字保留条件 ID、描述、必需标记和可选验证方法，并绑定 Spec ID、Spec revision、审核 Attention、审核 StageRun、审核 Session 和审核人。这个输入只是可重建的冻结视图，不是第十一个持久业务对象。修改其中任意条件或提交新的 Spec revision 都会使旧输入失效；新 Spec 必须重新经过审核。没有验证方法的条件明确标为 `verification_blocked` Attention 要求，不能被系统补写一个方便通过的检查。

### DeliveryTask 与 Codex Plan

`Codex Plan` 是一个线程为完成当前工作采用的执行步骤。WinWinCode 只读展示，不拥有状态，也不把每次 Plan 更新同步成工单。

只有满足以下至少一项的工作单元才成为 `DeliveryTask`：

- 可以独立验收；
- 可以独立失败和重新执行；
- 需要独立负责人或外部依赖；
- 产生独立交付物；
- 需要独立人工决定；
- 需要作为交付单元并行推进。

`DeliveryTask` 不包含 Codex 子 Agent、工具调用或思考步骤。

### StageRun 与 SessionBinding

`StageRun` 只记录一次交付阶段的责任类型、角色、尝试、状态和时间。`SessionBinding` 精确关联 `ProductSessionId`、`ExecutionJobId`、可选 `WorkerSessionId` 和可选 `CodexThreadId`。这些身份各有自己的生命周期，不能压成一个通用 Session ID。WinWinCode 不从这些对象创建自己的 Agent 生命周期；实际创建、恢复、分叉、等待、转向和中断继续由 Codex Core 完成。

交付状态采用以下主路径：

```text
Draft → Clarifying → Ready → Planning → Plan Review
  → Executing → Verifying → Ready To Deliver → Delivered
                         ├→ Reworking → Verifying
                         └→ Needs Attention
```

状态只回答交付层下一步是什么。Codex 内部一次命令重试、子 Agent 等待或上下文恢复不会生成新的交付阶段。

`startStage()` 同时承担明确的阶段交接：如果当前有正在运行的 Codex 阶段，它必须已经绑定精确的 `ExecutionJob`、`WorkerSession` 与 `CodexThread`；服务在同一次原子变更中结束当前 `StageRun` 并开始下一项 `StageRun`，不会另建阶段调度器。计划审核和最终交付审核只能由 human `StageRun` 开始，并且必须同时创建与该 `StageRun` 绑定的开放阻塞 `AttentionItem`。审核完成前状态为 `Needs Attention`；人工身份和 `ProductSession` 都验证通过后，才可以进入 `Executing` 或 `Delivered`。

被其他 `DeliveryTask` 阻塞的任务不能开始。任务级执行或返工进入验证时，验证阶段必须继续指向刚刚产出候选结果的同一个 `DeliveryTask`；没有独立交付子单元的单一 Delivery 则在这些阶段保持 `deliveryTaskId: null`，不会为了返工虚构一个任务。这些检查只约束交付阶段的流转，不接管 Codex 内部如何安排 Plan 或子 Agent。

返工只能使用 Codex `StageRun` 和 `remediator` 角色。它可以指向一个具有独立交付意义的 `DeliveryTask`，也可以在没有这种子单元时属于整个 Delivery。每次开始返工都会增加当前 `DeliverySpec` 的已用次数；角色不能伪装成普通 Executor，调用方也不能修改 StageRun 的 attempt。返工开始后，旧候选立即失效；新候选必须绑定新的 StageRun 和 Session，并重新经过 Reviewer、Verifier、逐项验收和最终 Verdict。返工期间 `DeliverySpec` 保持原样；确实需要改变目标、范围或验收条件时，必须进入 `Clarifying`，提交新的 Spec revision，再经过规划和人工审核。

### Attention

`AttentionItem` 只表示会影响交付流转的业务问题：需求问题、不可逆决定、验证被阻塞、范围变化和最终交付批准。Codex 的命令、文件、网络和权限审批仍由 Codex 与 DSH 原路径负责。

Plan Review 的人工决定使用 `winwincode.plan-review-decision.v2` 协议片段写入同一个 `AttentionItem.resolution`。决定必须再次携带当前 Spec ID/revision、审核 `StageRun`、Attention ID 和审核集合 SHA-256。`approve` 进入执行，`request_changes` 返回规划，`reject` 返回需求澄清；在这三种决定之一通过服务校验前，执行阶段保持锁定。旧页面、错误 Session、被改写的方案或图都不能完成审核。

验证结束后，调用方不制作 Attention。服务从当前 `DeliveryVerdict` 派生一组有界事项：失败的必需条件要求返工；`infra_error` 只要求在候选不变时重试验证；证据不足要求补齐验证；Reviewer 与 Verifier 冲突要求裁决后重验；没有对应验收条件的 finding 要求返回范围澄清。每项只保存当前 Verdict、Candidate、验证 StageRun、CriterionResult、EvidenceRef 和未解决 finding 的身份或集合摘要，不复制命令输出、会话全文或模型解释。

这些事项使用 `winwincode.delivery-attention.v1` 上下文并由当前 Verdict 确定性重建。上下文同时记录返工已用次数、上限和是否为同一条件重复失败。解决时服务重新计算并逐字段比对；缺项、改写、旧候选或旧 Verdict 都会失败。若仍有其他阻塞项，Delivery 保持 `Needs Attention`；全部解决后，范围问题进入 `Clarifying`，代码失败进入 `Reworking`，验证故障或证据问题回到 `Verifying`。同一条件返工后再次失败，或批准的返工次数已经用尽时，不再生成 `start-rework`，而是进入需求定义复核。调用方不提交 `nextStatus`，人工身份和决定会写入现有 `AttentionItem`。

最终交付审核的图上标注通过 `winwincode.delivery-remediation.v1` 请求进入同一条 Attention 路径。每项标注绑定当前冻结候选、Diff SHA-256、系统架构图或流程图、稳定节点、变化文件、hunk SHA-256 和当前 EvidenceRef；路径不属于候选 Diff、证据属于旧候选或候选已经被后续写入替代时，请求失败。服务只把摘要、这些精确身份和标注保存到现有 `AttentionItem.resolution`，不复制完整候选、Diff 或运行日志，也不新增 Remediation 业务对象。有效标注把指定的已完成 `DeliveryTask` 置为返工状态，随后仍由新的 remediator Codex Session 完成实际修改。

面向 GitHub Pull Request 的最终审核使用 `winwincode.github-publication-context.v1`。服务从当前 Delivery 生成该上下文，绑定来源 Issue、唯一 PR 目标、Spec ID/revision、冻结候选引用、通过的 Verdict、人工审核 StageRun 与 Attention，并为同一 Delivery 和目标生成稳定的 provider idempotency key。整个集合再计算一个 SHA-256。人工批准使用 `winwincode.github-publication-decision.v1` 写入同一个 Attention resolution；决定逐项重复关键身份和集合摘要。服务在进入 Delivered 前重算并核对全部内容。后续发布适配器只能消费已经解决且仍与当前候选一致的绑定，Spec、候选、Verdict、目标或批准任一变化都会在远端调用前失败。

长讨论、Mention、项目看板、日程和组织协作继续留在 GitHub、Jira、Linear、Slack 或 Teams。WinWinCode 只保存外部引用、当前责任、决定和阶段影响。

### GitHub Issue → Delivery → Pull Request

GitHub 仍是外部工作和协作事实的所有者。WinWinCode 的第一层绑定只有以下内容：

```text
sourceRef: owner/repository#issue
Delivery ID: dlv_<26 Crockford Base32 characters>
publicationTarget: head repository/branch → base repository/branch
publication approval: exact Spec + Candidate + Verdict + target + human decision
```

GitHub Issue 的规范身份输入固定为：

```text
winwincode.github-issue-delivery-id.v1\0github\0issue\0<lowercase owner/repository>\0<number>
```

调用方可以重复提交来源，但不能为来源指定另一个合法 `dlv_` 身份。TypeScript 与 Rust 都重新计算并逐字比对。`publicationTarget` 是单值，Spec 更新不能把同一个 Delivery 改到另一项 Pull Request。provider idempotency key 由 Delivery、来源和目标确定，因此候选返工后仍指向同一项预期 PR；新的候选和 Verdict 会产生新的审核集合摘要和人工批准。

在真实提供商调用前，`generateStrongFlowGitHubReviewPackage()` 从当前 Delivery、冻结候选和 DSH/Codex RuntimeEvent 生成 `winwincode.github-review-package.v1` 本地派生包。固定目录把需求、批准方案、方案决定、系统架构图、流程图、候选、Diff、EvidenceRef、CriterionResult、DeliveryVerdict、发布审核上下文和 Pull Request 预览分开。包内不复制原始 Session 日志；Evidence 仍然只保存来源引用。生成时每个引用都必须能从当前候选或原始运行事件重建，结束图也必须由同一个候选 Diff 和已批准图重新投影。

Manifest 保存每个文件的 SHA-256 与字节数，并绑定 Spec、方案审核集合、候选、Verdict、发布审核集合和稳定 provider key。离线读取会重算所有摘要，并检查方案、两张图、决定、Evidence、验收结果、Verdict、发布上下文和 PR 预览之间的身份。相同事实得到相同 Package ID 和预览。写入使用本地临时目录和原子重命名；已有同一 Package ID 会直接复用，不同内容会报告冲突。

`github/dry-run.json` 固定记录 `publicationOccurred: false` 和 `remoteWriteCount: 0`。包生成接口不接收 GitHub 客户端，也没有 branch、Pull Request、release、comment 或 status 写入能力，因此本步骤只形成正式人工审核依据。Review Package 属于可重建文件协议，不写入 Delivery，也不增加新的业务对象；live 发布协调器只消费通过校验的审核包和人工发布决定。

发布入口 `runStrongFlowGitHubPublication()` 默认采用 `dry-run`，即使调用方传入 provider 也不会执行查询或写入。`live` 必须显式指定，并重新验证 Delivered 状态、当前人工发布决定、Spec、Candidate、Verdict、Review Package、来源和目标。任一内容过期都会在 provider 调用前结束。

通过校验后，协调器一次性派生四项有序操作：head branch、Pull Request、来源 Issue comment 和 candidate commit status。每项都有稳定 operation key 和请求摘要。provider adapter 由 DSH 或提供商插件注入，负责使用原有认证边界，并保证相同 operation key 的 apply 收敛；WinWinCode 不增加凭据设置或 GitHub 账号存储。

协调器先把完整发布意图写入 `$DSH_HOME/winwincode/github-publications` 下的追加式 operational journal，再为每一项执行 lookup、记录 apply intent、调用 provider、记录结果。调用抛错、返回不明确或结果格式异常都记录为不含原始错误文本的 `unknown`，当前运行返回 pending。下一次运行先 lookup：找到同一请求摘要就补记成功；仍然不明确就继续 pending；明确缺失后才再次 apply。这样即使进程在远端成功后、本地结果落盘前结束，也能通过 provider 查询恢复。

同一 Package 的并发运行可能重复只读 lookup 或幂等 apply 调用，但不会确认第二份 branch、Pull Request、comment 或 status；journal 的成功事实也不会被后来的 unknown 结果降级。Provider 返回的资源引用在落盘前经过严格结构和认证材料检查。这个 journal 只记录外部副作用的意图与对账事实，不写入 Delivery，不成为第十一个业务对象，也不接管 GitHub 的 Issue、PR、评论或状态所有权。

### Evidence 与 Verdict

`EvidenceRef` 保存类型和来源引用，不复制整份 Codex 或 DSH 日志。`CriterionResult` 对一个验收条件给出 `pass`、`fail`、`inconclusive` 或 `infra_error`，并引用直接依据。`DeliveryVerdict` 汇总当前 `DeliverySpec` 和候选结果的全部条件。

已有 Git 冻结结果先被解释成一份可重建的 `FrozenDeliveryCandidate` 值。它绑定当前 Delivery、Spec ID/revision、仓库与基线、产出候选的 `StageRun` 和 `SessionBinding`，并保留 base/candidate commit、tree、精确 diff 摘要和变化路径对象。候选引用由这些字段共同计算；这份值不写入 Delivery，也不是第十一个业务对象。后续 Spec 更新、字段修改或新的执行/返工运行都会使旧值失效。

证据解析只消费两类已经存在的事实：上述 Git 冻结事实，或 `RuntimeSessionLedger` 中可以由语义投影重新找到的 Codex 事件。每个持久 `EvidenceRef` 明确保存当前 Spec revision、`StageRunId`、`SessionBindingId` 和候选引用；`sourceRef` 指向原始 Git 对象、文件、diff 或 RuntimeEvent。解析时逐项核对来源存在、类型一致、完整 DSH/Codex Session 身份一致，以及 Diff、文件、提交或评审发现属于同一候选。解析器不启动命令、测试或 Agent，也不复制 stdout、stderr 和完整日志。

命令与测试的结果保持为可解释类别：成功、任务失败、超时、策略拒绝、基础设施失败和取消不会被压成同一个“失败”。Reviewer、Verifier 和 Adversarial Verifier 必须使用 Codex 的只读候选策略；如果其事件中出现成功的文件或补丁写入，证据解析会失败，而且候选引用仍只来自原执行者的冻结结果。

Reviewer 与 Verifier 各自使用一个角色明确的验证 `StageRun`，并绑定各自已经由 DSH 和 Codex 创建的 Session。验证阶段可以在同一个 DeliveryTask 和候选上从 Reviewer 交接到 Verifier；每个角色的尝试次数独立计算。可选的 Adversarial Verifier 使用同一条交接规则。WinWinCode 不从这些记录创建 Agent，也不建立参与者名单或消息队列。

每个验证 Session 的第一轮输入由当前已批准的完整 `DeliverySpec`、冻结的验收输入、冻结候选和结构化结果格式共同生成。这个输入和 Session 关联值都可以从已有事实重建，不写入第十一个业务对象。Reviewer 和 Verifier 至少都是必需角色；Adversarial Verifier 可以被提升为必需角色，但调用方不能通过配置删除前两者。

验证结果使用 Codex 的最终 `AgentMessage`，内容必须是 `winwincode.independent-verification-result.v1` 严格 JSON。一个回复可以携带多项 finding；投影层逐字段校验根身份、每项结论和它引用的更早 Codex 事件。当前 DSH 模型接口没有跨提供商的响应 Schema 参数，因此投影层对最终消息做严格校验：格式错误的回复不会成为结果，阶段只会显示为未形成有效结论。

结构化评审发现进入 RuntimeEvent 时保留 finding、Spec ID/revision、candidate、criterion、verdict、说明和直接证据事件。投影逐项重查这些身份与证据来源。没有对应 StageRun、没有绑定、仍在运行、已结束但没有结构化结果、失败和取消是不同状态，不会被一个“已完成”布尔值掩盖。同一验收条件出现不同结论时，所有原始发现都保留，并额外给出只含来源事件和不同结论的确定性冲突记录，供最终 Verdict 计算使用。

每个验证 Session 的 `runtimeSession` 直接来自现有 `DeliveryRuntimeProjection`。其中的 Agent 节点和连线仍是 Codex Agent Graph 的只读投影；独立 Reviewer/Verifier 流程没有创建第二张 Agent 图。

通过结论必须满足：

1. 每个当前验收条件都恰好有一项结果；
2. 每个必需条件为 `pass`；
3. 所有依据属于当前 Delivery、Spec 和候选结果；
4. 没有会改变验收结论的未解决阻塞项；
5. 结论由程序校验，而不是来自 Agent 的完成消息。

`submitVerdict()` 的请求不携带调用方制作的 `EvidenceRef`、`CriterionResult` 或
`DeliveryVerdict` 或业务 `AttentionItem`。它只携带当前冻结候选、DSH 已规范化的 Codex
事件和必需验证角色。服务重新冻结当前验收输入，校验候选、角色 Session、只读权限、
事件来源与直接证据，再确定每项条件和总结果。缺少角色或结果、仍在运行、证据不足、
通过结论引用失败检查、相互冲突、基础设施错误、过期 Spec/候选以及开放阻塞项都会关闭
通过路径。同一输入会生成相同 Evidence、CriterionResult 和 Verdict 身份；原始事件只参与
计算，不写入 Delivery 存储。非通过结论所需的 Attention 由服务从这批已验证事实生成。

最终交付批准不改写已经通过的验证结论。它单独阻止 Delivery 进入 `Delivered`；批准后才交付，带有效结构化标注的驳回会清除当前结论并进入有次数限制的 `Reworking`。

## Codex 语义投影

现有 `RuntimeSessionLedger` 继续保存 DSH Session 与 Codex Session 的映射和原始顺序事件。新增层只解释事件，不建立第二份执行记录：

```text
Codex Runtime Events
        ↓
Delivery Projection
        ├─ Structured Plan
        ├─ Agent Graph
        ├─ 当前 StageRun 活动
        ├─ Diff 与变更文件
        ├─ Command / Test evidence
        ├─ Failure / recovery
        ├─ Approval
        └─ Usage
```

`plan_update`、`plan_delta` 和 Plan Item 生命周期统一成为带类型的 `plan.updated` 事件，不再降成普通聊天文本。Agent 事件保留 Codex Thread ID、路径、父节点、角色和状态。`request_user_input` 与命令执行审批分开，前者只生成可供 Delivery 业务层判断的 Attention 候选，后者仍由 DSH 原有交互处理。

`DeliveryRuntimeProjection` 只接受当前 `SessionBinding` 同时匹配的 DSH 与 Codex Session。它把事件按 `SessionBinding` 汇总到 `StageRun` 和 `DeliveryTask`，保留当前 Plan、Agent 图、命令与测试状态、最新 Diff、用量、失败恢复、交互以及指向原始事件的 Evidence 引用。它不复制命令输出或运行日志，不写 Delivery，不调用 Codex，也不建立新的 Plan、Agent、Tool 或 Session 存储。进程重启后从 `RuntimeSessionLedger` 顺序重放会得到相同视图；内存里只保留有限数量的近期事件指纹用于识别重复输入。

### 重启协调

`reconcileDeliveryAfterRestart()` 是只读协调边界。它先从 `DeliveryStore` 读取最新完整快照并校验 Spec、阶段、任务、Attention、Evidence 和 Verdict 的关系，再逐个打开 Codex `StageRun` 对应的 `RuntimeSessionLedger`，核对 DSH Session、Codex Session 和角色身份，并重放出 DSH 与 StrongFlow 投影。调用方已有重建后的 `FrozenDeliveryCandidate` 时，协调器还会核对它是否仍属于当前 Spec 和最新写入阶段；候选本身仍然不写入 Delivery。

协调器只通过 Codex 的公开 `listSessions()` 查询当前加载的 Thread，不执行 Tool、不重放命令，也不直接恢复 Thread。结果始终只有一个交付层动作：处理业务 Attention、创建或恢复阶段 Session、处理原执行审批、继续当前阶段、审核已经结束但尚未提交的阶段输出、开始 Delivery 状态明确要求的新阶段，或确认 Delivery 已结束。模型最终回复即使写着“完成”，只要 `StageRun` 仍是活动状态，就保持为待审核输出；已落库的完成阶段不会被选为恢复目标。绑定重复、Ledger 身份冲突、已结束阶段残留审批或多个活动阶段都会作为可见冲突停止协调。

## StrongFlow Host 边界

对外主模块只有以下交付操作：

```text
createDelivery()
updateDeliverySpec()
startStage()
bindSession()
resolveAttention()
submitVerdict()
getDeliveryProjection()
```

DSH Remote 与 CLI 都通过同一个 `StrongFlowServiceInvoker` 调用这些操作。CLI 使用 `winwincode delivery ...` 命令；浏览器使用 `strongflow/invoke` Remote。二者读取同一份 Delivery 记录，没有各自的状态文件。

发布包的 `winwincode` 入口会创建并规范化 `winwincode` DSH profile，以 DSH base、DSH Web 和 `@winwincode/dsh-profile` 三层启动原始 Chat 界面；`winwincode web` 接受 DSH Web 参数。DSH Host 与 Delivery CLI 都以 `$DSH_HOME/winwincode` 作为同一个持久目录。启动器把 `SIGINT`/`SIGTERM` 传给 DSH 子进程并返回 `130`/`143`；Delivery CLI 另外用固定退出码区分参数、未找到、revision/状态冲突和本地服务故障。发布校验从 tarball 覆盖出的独立安装启动真实 Web 两次，并用分离的 CLI 进程检查开放 Attention 在中断和重启后仍可见、可认证解决。

每次变更都带 `requestId` 和预期 revision。服务把完整 Delivery 快照写成按摘要相连的追加记录，并用原子文件发布。相同请求可安全重试；相同请求身份不能表示另一项变更；过期 revision 在写入前失败并返回当前 revision。重启只从这些记录恢复 Delivery，不从聊天或 Agent 完成消息猜测结果。

重启回归会在 `createDelivery`、`updateDeliverySpec`、`startStage`、`bindSession`、`resolveAttention` 和 `submitVerdict` 的调用前后重新创建 Host service，并再次提交完全相同的请求。每个 `requestId` 最终只对应一条记录；阶段、绑定、Attention、Evidence 和 Verdict 不重复；返工计数只增不减；经历失败、返工和重新验证后的最终快照与不中断执行完全一致。

解决业务 Attention 需要本地 DSH Session 或 CLI peer proof。Plan Review 还要求调用来自与审核 `StageRun` 精确绑定的 DSH Session。浏览器只发送当前 Session 的安全引用；DSH Host 先核对 Session、Delivery revision、`StageRun` 与 `SessionBinding`，再在进程内换成临时证明。证明不会下发到浏览器，也不进入请求摘要、Delivery 记录或返回结果。其他持久内容在写入前拒绝原始凭据材料。

旧的 Job 合同、操作服务、状态存储、固定 Agent 编排、制品总线和对应发布文件已经删除。当前没有 `job.*` 操作、旧命令别名、双读、双写或第二个 StrongFlow 状态入口。

## 界面与评估

StrongFlow 看板是 Delivery 与 Codex 事件的视图，不是新的任务后端。默认 DSH Chat、模型设置、凭据、普通 Session 和执行审批界面保持原有所有权。

StrongFlow 工作台通过 DSH 已有的 `conversation.view` 插槽进入会话界面，并继续使用已有的 `strongflow/invoke` Remote 和 `ctx.sessions.open()`。Chat 保持默认入口；StrongFlow 只作为用户主动选择的高级视图。工作台可以通过唯一的 `createDelivery()` 操作创建 Delivery，也可以按 ID 读取 `getDeliveryProjection()`，展示 Spec、验收条件、Delivery Task、阶段、Session 绑定、Attention、Evidence 和 Verdict。创建界面不会把 Codex Plan 步骤提升为 Delivery Task。

Plan Review 页面按固定顺序把需求定义、验收条件、方案、系统架构图、流程图、风险、未决事项和决定区分开展示。两张图同时提供稳定节点/关系身份和可读清单，不依赖颜色或画布才能理解。决定区使用原生表单控件、可见键盘焦点和状态播报；只有审核 `StageRun` 绑定的 DSH Session 可以编辑和提交，其他 Session 只能查看并跳转到正确 Session。

工作台定时刷新 Host 返回的完整 Delivery 投影。浏览器仅按 DSH Session 保存最后选择的 Delivery ID，作为返回 Chat 后恢复界面的偏好；它不保存可独立修改的 Delivery 副本。执行活动通过 `SessionBinding` 链接回现有 DSH Session，Plan、Agent Graph、工具调用和原始事件继续从 Codex/DSH 所有的 Session 读取。这样界面刷新或切换模式不会产生新的调度器、Agent roster、mailbox、Session 日志或执行状态。

当前工作台已经完成 Delivery 创建、跟踪、需求/方案分离展示、默认图、精确人工 Plan Review 和图上执行审核。系统架构图与流程图保持 Plan Review 已批准的节点和顺序，并由可重建的 `winwincode.diagram-execution-projection.v1` 视图表达三种状态：执行前所有节点为绿色正常流转状态；执行中由绑定 Session 的规范化 Diff 事件把受影响节点更新为浅蓝色，返回值的 `details` 固定为空；执行结束后从当前冻结候选与 SHA-256 完全一致的权威 Diff 重新解析文件和 hunk，把受影响节点设为黄色并开放详情。状态同时使用文字和图标表达。

执行结束详情绑定当前 Delivery revision、审核集合摘要、候选、Diff、生产 `StageRun`、`SessionBinding`、角色、次数、DSH/Codex Session、Agent 活动、命令、测试、Evidence 和时间。文件只能通过审核方案中每个组件的仓库相对路径前缀映射到架构节点；流程节点由生产阶段确定。人工标注必须精确命中当前黄色节点中的候选、图、文件和 hunk，随后作为 `winwincode.delivery-remediation.v1` 写入现有 `AttentionItem.resolution` 并进入下一次有上限的返工。图投影不写入 Delivery；页面刷新和主机重启都从 Delivery、Plan Review 内容以及 DSH 的 `RuntimeSessionLedger` 重建。

评估先保留五项可解释数据：

- 验收条件完成情况；
- 证据类型和独立验证结果；
- 失败、重试、中断和恢复事实；
- Attention 与人工决定次数；
- 时间、Token、模型调用和 Codex 并行度。

第一版不生成没有可追溯计算依据的总分。

## 不在当前底座范围内

Organization、RBAC、共享数据库、SSO、Jira、Slack、Teams、多进程服务和审计导出属于真正进入多人服务器阶段后的工作。当前实现不为商业化预先复制这些平台。
