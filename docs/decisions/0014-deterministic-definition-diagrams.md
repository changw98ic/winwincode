# ADR-0014：两张定义图由结构化方案确定生成，不执行模型图形标记

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.6.5`
- 实现：[`packages/strongflow/src/definition-diagrams.ts`](../../packages/strongflow/src/definition-diagrams.ts)
- 测试：[`tests/strongflow-definition-diagrams.test.mjs`](../../tests/strongflow-definition-diagrams.test.mjs)

## 结论

每一份通过校验的 `SolutionDesign` 都能由 `generateStrongFlowDefinitionDiagrams` 一次生成两项不可分割的结果：

- `SystemArchitectureDiagram`：显示当前方案中的组件、职责、连接、信任边界、外部系统和未确认信息；
- `ProcessFlowDiagram`：使用产品固定流程显示需求整理、方案设计、两张图生成、人工审核、退回修改、批准、拒绝、计划、执行、审查、验证、修复、完成门禁和交付。

系统图的任务内容只来自已经校验的方案组件和连接。流程图的产品阶段和边使用代码中的固定模板。模型不提交 Mermaid、SVG、HTML、脚本、链接或布局代码。

## 稳定节点和布局

系统图节点 ID 从组件或未确认项的稳定 ID 计算，流程图节点使用固定产品 ID，例如 `process:10-execution`。相同需求和方案会产生完全相同的节点、边、布局 ID、Mermaid、SVG 和 SHA-256 摘要。

`validateStrongFlowDefinitionDiagramPair` 会重新生成预期内容并逐项比较。它只接受：

- 同一个作业中的当前需求和当前方案；
- 同一次 Solution Architect 角色回合产生的两张图；
- 两个不同的 `DiagramId`；
- 正确引用当前 `RequirementId` 和 `SolutionId` 的两张图；
- 与内置系统图映射和流程模板完全相同的结构。

模型改标签、漏阶段、换来源、改方案 ID、改变内核回合或只交一张图，都会在进入界面前失败。

这些稳定节点和 `layoutId` 会被后续执行图层直接复用。执行前全部节点显示绿色“正常流转”；执行中只把有变化的既有节点改成浅蓝色；候选版本冻结后再把有变化的同一节点改成黄色。不会重新生成第三张差异图。

## 未确认信息

需求中的待确认问题和方案中的未确认事实不会被补写成猜测。系统图为每一项生成带问号、虚线和 `unresolved: true` 的节点；流程图在定义图与人工审核之间增加一个同样明确的“待确认信息”节点。没有未确认项时，这个节点和对应两条边会确定地消失，流程直接进入人工审核。

所有节点在执行前仍使用绿色底色。未确认状态另外使用问号、文字和虚线表达，因此不只依赖颜色。

## 安全渲染

`renderStrongFlowDefinitionDiagram` 只读取已通过正式制品校验的节点和边，再由程序生成：

- 不含点击命令和初始化指令的 Mermaid；
- 不含脚本、事件处理器、外部链接、外部资源或 `foreignObject` 的独立 SVG；
- `role="img"`、标题、说明、节点键盘焦点和完整 `aria-label`；
- 图、需求、方案、布局和稳定节点的数据属性；
- Mermaid 和 SVG 各自的 SHA-256，供后续证据包核对。

普通的尖括号、引号和与号会转义。看起来像脚本、外部资源、URL 协议或事件处理器的主动标记，以及超出节点、边和文字上限的内容，会在渲染前失败。

本任务提供可直接放入 DSH 工作台和证据包的安全结果。工作台页面的实际接入由 `winwincode-9c4.9.5` 完成；三种执行状态和差异详情由 `winwincode-9c4.9.6` 完成。
