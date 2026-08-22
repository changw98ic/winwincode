# ADR-0013：所有角色、人工审核和图上标注只使用一套制品格式

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.6.1`
- 实现：[`packages/contracts/src/strongflow-artifact.ts`](../../packages/contracts/src/strongflow-artifact.ts)
- 角色接入：[`packages/strongflow/src/artifact-validator.ts`](../../packages/strongflow/src/artifact-validator.ts)
- 测试：[`tests/strongflow-artifact.test.mjs`](../../tests/strongflow-artifact.test.mjs)

## 结论

StrongFlow 只有一套版本为 `1` 的制品格式。它覆盖用户请求、需求、方案、系统架构图、流程图、人工审核、执行计划、变更清单、代码审查、验证、修复请求、修复报告、交付回执和图上变更标注。

每个完整制品都必须带有：

- 不可变的 `artifactId` 和 `jobId`；
- 按固定顺序列出的准确来源制品；
- 生产它的角色、人工审核人或系统身份；
- 角色实际执行时对应的 Codex 内核会话、回合和连续事件范围；
- 创建时间和唯一的 `schemaVersion`；
- 该制品种类自己的严格内容字段。

任何未知版本、多余字段、错误生产者、来源缺失、来源换序、身份对不上、内核事件范围不连续，或者内容内部引用不存在，都会直接失败。格式不提供旧版兼容读取，也没有第二套宽松格式。

## 同一个校验入口

完整制品在原生接口、磁盘、界面和命令行边界都调用 `parseStrongFlowArtifact`。这个函数会重新建立一个深度冻结的对象，不直接信任调用方传入的对象。

模型只负责产出当前制品的 `payload`，不能自行选择制品 ID、作业 ID、来源、角色身份、时间或内核证据。`createStrongFlowCanonicalRoleArtifactValidator` 从已安装的角色上下文和实际内核事件中加入这些信息，再交给同一个完整制品校验入口。这样，模型输出和其他入口最终使用的是同一套规则。

## 需求和方案必须分开

`RequirementSpec` 只允许目标、非目标、限制、验收条件、已经核实的仓库事实、风险和待确认问题。它不允许 `solutionDesign`、组件、连接、文件、命令、补丁或审批字段混入。

`SolutionDesign` 必须只引用一个准确的 `RequirementId`，并且这个 ID 必须等于它唯一的需求来源。两个定义图必须同时引用这份需求和这份方案。

人工审核记录现在也是完整制品，不再使用早期的轻量对象。批准、要求修改或拒绝必须引用同一组四个定义制品：需求、方案、系统架构图和流程图。作业状态中的 `approval` 和 `lastHumanReview` 也只保留这一个正式格式。

## 图和执行变更

系统架构图和流程图保存结构化节点与边，不保存 Mermaid、SVG 或任意脚本文本。系统节点还明确记录信任边界，所有节点明确记录是否为未确认信息。节点必须有稳定 `nodeId`，边只能连接当前图中存在的节点。这些稳定 ID 是后续生成执行前、执行中和执行结束三种图状态的依据：

1. 执行前：所有节点为绿色；
2. 执行中：有变化的节点实时变为浅蓝色，界面不开放具体变更；
3. 执行结束：有变化的节点变为黄色并可查看准确文件、变更块和说明。

`ExecutionChangeAnnotation` 记录已经登录的人工审核人、当前候选版本、Git 差异、变更清单、图、稳定节点，以及可选的准确文件和变更块。`requireCurrentExecutionChangeAnnotation` 会再次核对当前候选版本、差异、图、节点、文件和变更块；任何一项已经变化，旧标注都不会进入新的修复请求。

图状态计算、图生成、实时更新和界面点击行为属于后续任务；本决定先固定它们依赖的稳定数据和拒绝过期标注的规则。

## 候选代码和验证证据

`PatchManifest`、`ReviewReport`、`VerificationReport`、`RemediationRequest`、`RemediationReport` 和 `DeliveryReceipt` 都携带完整候选版本身份：源快照、基准提交和文件树、候选提交和文件树、以及 Git 差异摘要。文件变更使用相对路径和稳定变更块 ID；命令与测试证据使用输出摘要，不能塞入任意未声明字段。

Reviewer 和 Verifier 可以报告结论，但这些报告不等于最终交付批准。交付回执只能由 StrongFlow 系统生成，并准确引用已批准定义、执行计划、最终变更清单、审查和验证报告。
