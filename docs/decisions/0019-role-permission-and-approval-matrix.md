# ADR-0019：角色权限与人工决定使用一份完整矩阵

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.5.1`
- 实现：[`packages/contracts/src/strongflow-permission.ts`](../../packages/contracts/src/strongflow-permission.ts)

## 结论

StrongFlow 不再把工具名单、文件权限和审批开关散落在不同调用处。每个模型角色只引用一个不可修改的权限预设；人工审核者和程序化最终确认器也各有一份完整预设。预设一次性说明：

1. 可以读写哪个工作区；
2. 模型能看到哪些工具；
3. 可以运行哪一类进程；
4. 网络是否可用；
5. 谁能提出和作出人工决定；
6. 如何消费或增加预算；
7. 可以写入哪些本地结果、能否远程发布；
8. 凭据如何引用；
9. 必须留下什么审计身份并怎样隐藏凭据。

所有字段都是必填项。缺字段、加字段、未知版本或修改内置值都会在角色开始前失败，不会把缺失值解释为允许。

## 固定矩阵

| 主体 | 权限预设 | 工作区 | 进程 | 本地写入范围 |
| --- | --- | --- | --- | --- |
| Requirements Analyst | `definition-read` | 分配的源码只读 | 禁止 | `RequirementSpec` |
| Solution Architect | `solution-read` | 分配的源码只读 | 禁止 | `SolutionDesign` 和两张定义图 |
| Planner | `source-read` | 分配的源码只读 | 禁止 | `ExecutionPlan` |
| Executor | `candidate-write` | 只写分配的候选工作区 | 只运行批准计划中的命令 | `PatchManifest` |
| Reviewer | `snapshot-verify` | 冻结候选快照只读 | 只运行批准的验证探针 | 评审和验证结果 |
| Verifier | `snapshot-verify` | 冻结候选快照只读 | 只运行批准的验证探针 | 评审和验证结果 |
| Adversarial Verifier | `snapshot-verify` | 冻结候选快照只读 | 只运行批准的验证探针 | 评审和验证结果 |
| Remediator | `remediation-write` | 只写分配的候选工作区 | 只运行修复请求允许的命令 | 新补丁清单和修复结果 |
| 人工审核者 | `human-definition-review` | 不直接访问工作区 | 禁止 | 人工审核记录 |
| 程序化最终确认器 | `deterministic-finalizer` | 不直接访问工作区 | 禁止 | `DeliveryReceipt` |

只有 Executor 和 Remediator 的文件模式是 `candidate-write`，也只有这两个预设包含 `candidate.patch`。其余模型角色对候选内容只读或不接触候选。

## 定义审核与运行权限决定分开

人工定义审核决定需求、方案和两张图能否进入执行。运行权限决定只处理以下五类具体请求：

- 网络放行；
- 额外的 DSH 凭据引用；
- 权限范围扩大；
- 预算增加；
- 远程发布。

模型角色可以提出这些请求，但没有定义批准能力、运行权限决定能力或自我批准工具。人工审核者可以分别作出定义决定和带来源身份的运行权限决定；作出一次定义批准不会同时放开网络、凭据、预算或发布。

程序化最终确认器不是模型角色。它只能根据精确身份和已有人工发布决定生成交付记录或执行已批准的发布，不能自行批准、扩大权限或取得凭据。

## 网络、凭据与发布

所有模型预设的默认网络状态都是 `disabled`。模型调用只携带 DSH 选择的 provider/model 引用，原始凭据不进入角色环境、提示、事件或制品。需要其他凭据时只能请求人工决定，批准后仍由 DSH 在模型调用时解析引用。

任何模型角色都不能直接执行远程发布。模型只能提出请求，人工审核者只能作出决定，真正的远程副作用只能由程序化最终确认器在精确批准和制品身份匹配时执行。

## macOS 与 Linux 的启动门

首发只接受以下四个主机：

- `darwin/arm64`；
- `darwin/x64`；
- `linux/arm64`；
- `linux/x64`。

角色或可信主体启动前，宿主必须证明文件限制、进程限制、网络限制、环境变量白名单、带来源身份的人工决定、DSH 凭据引用、发布身份保护和持久化脱敏审计全部可用。任一项缺失或只实现了一部分都会返回明确错误；不降级到无沙箱运行，也没有 `danger-full-access` 预设。

## 执行状态

`winwincode-9c4.5.2` 已把模型角色权限安装到每个 Codex Core 会话，具体实现和失败顺序见 [ADR-0020](./0020-governed-role-kernel-authority.md)。`winwincode-9c4.5.3` 已完成受管进程的 macOS/Linux 沙箱、网络拒绝、环境变量白名单、DSH 凭据引用边界、持久化脱敏审计和四平台安装验证，详见 [ADR-0021](./0021-governed-process-credential-and-audit-boundaries.md)。
