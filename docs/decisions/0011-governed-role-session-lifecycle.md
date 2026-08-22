# ADR-0011：角色上下文安装完成后才发布内核会话

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.3.2`
- 实现：[`packages/strongflow/src/role-session.ts`](../../packages/strongflow/src/role-session.ts)、[`packages/strongflow/src/role-authority.ts`](../../packages/strongflow/src/role-authority.ts)
- 测试：[`tests/strongflow-role-session.test.mjs`](../../tests/strongflow-role-session.test.mjs)、[`tests/strongflow-role-authority.test.mjs`](../../tests/strongflow-role-authority.test.mjs)

## 结论

StrongFlow 通过 `StrongFlowRoleSessionManager` 创建和恢复八个角色的 Codex Core 会话。控制器提供 `JobId`、`StageRunId`、`AttemptId`、固定角色和由工作区管理器产生的目录分配；管理器从已经过 DSH 模型目录校验的八角色配置中取出完整 `RoleSpec`，不接受调用方临时拼接提示词、工具或预算。

一个角色会话按以下顺序建立：

```text
校验作业、尝试、角色和工作区
             │
             ▼
从 RoleSpec 和固定权限矩阵生成不可修改的内核权限信封
             │
             ▼
在 Codex 线程启动前一次性设置 cwd、模型、指令、工具和限制
             │
             ▼
启动后读取 Codex 实际配置；逐项核对完整证据，失败就关闭线程
             │
             ▼
开始读取有顺序的事件，并安装工具与人工决定处理器
             │
             ▼
原子保存上下文、内核映射、实际权限证据和进程所有权
             │
             ▼
发布给 StrongFlow 控制器
```

权限信封设置、实际证据核对、安装、事件订阅或保存中任一步失败，管理器都会关闭刚创建的内核会话并释放安装器返回的资源。失败会话不会进入可查询的活动会话列表，也不会留下已接受的会话目录。恢复走同一条顺序，不会从 rollout 恢复出另一套默认工具。

## 两种内核身份

`StrongFlowKernelSessionLineageId` 由以下四项按长度编码后计算 SHA-256：

- `JobId`；
- `StageRunId`；
- `AttemptId`；
- `StrongFlowRoleId`。

这个标识是一次角色分配的稳定查找键。Codex Core 自己返回的 `KernelSessionId` 仍保留为实际线程身份。两者通过持久化的生命周期记录关联，不把随机的 Codex 线程标识伪装成确定性标识。

每次创建或恢复还会计算：

- `StrongFlowRoleSpecId`：完整不可变 `RoleSpec` 的内容身份；
- `StrongFlowRoleContextId`：会话沿袭标识、角色规范和规范化工作区分配的内容身份；
- `kernelStreamId`：沿袭标识、恢复代数和实际 `KernelSessionId` 的内容身份。

同一个角色分配恢复后保持相同的沿袭标识和上下文标识，但增加恢复代数并建立新的事件流标识。不同角色即使属于同一个作业和尝试，也不会共用上下文。

## 持久化和恢复

每个沿袭标识只有一个目录：

```text
HOME/strongflow-role-sessions/SHA256/
├── context.json       完整且不可变的角色和工作区快照
├── lifecycle.jsonl    创建、恢复和最终结束记录
└── owner.json         当前进程所有权；正常结束后删除
```

首次创建先在临时目录写完并同步，再通过同目录原子重命名发布。每条 `kernel.accepted` 记录都保存内核实际报告的权限证据，包括角色、权限预设、工作区、工具名单、文件模式、网络、进程入口、环境变量继承、人工决定策略和指令来源。恢复时重新使用当前 DSH 模型目录校验八角色配置，然后逐字核对保存的角色规范、旧证据和当前权限矩阵，并重新解析工作区真实路径。角色预算、指令、工具、沙箱声明、模型路线、工作区或证据中任一项变化都会在发布恢复会话前失败。

活动所有权记录包含进程号。仍存活的所有者会阻止第二次恢复；进程已经消失时，新管理器可以回收旧所有权并从最后一个 rollout 恢复。已经记录 `completed`、`cancelled` 或 `failed` 的会话是终态，不能再次恢复。

## 事件和资源所有权

管理器在发布会话前启动唯一的内核事件订阅，并为事件加上沿袭标识、上下文标识、恢复代数、实际内核会话和事件流标识。事件序号必须严格递增。管理器使用有上限的内存队列；即使调用方没有读取且队列已满，取消或结束也会先关闭队列，从而不会卡住资源清理。

角色上下文安装器是强制入口。它接收冻结的完整上下文、内核实际权限证据和 `AbortSignal`，必须返回相同的 `ContextId`、内核事件处理函数以及资源释放函数。事件处理函数先处理动态工具或 Codex 原生审批，再把事件交给角色运行器；模型看不到批准工具，也不能绕过宿主处理器。后续角色运行器只能使用这里保存的预算，不能在开始工作后换模型、改指令或扩大上限。

取消会先打断准确的 Codex 会话，再关闭会话、停止事件读取、释放角色拥有的工具、文件和进程，最后记录终态并删除活动所有权。正常结束走同一条清理路径，但不额外发送打断请求。任何清理失败都会得到明确的 `TEARDOWN_FAILED`，并把会话标为失败，而不是报告正常完成。
