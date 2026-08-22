# ADR-0020：角色工具和人工决定必须由宿主在内核边界执行

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.5.2`
- 实现：[`crates/kernel/src/lib.rs`](../../crates/kernel/src/lib.rs)、[`packages/strongflow/src/role-authority.ts`](../../packages/strongflow/src/role-authority.ts)、[`packages/dsh-profile/src/strongflow-approval.ts`](../../packages/dsh-profile/src/strongflow-approval.ts)
- 测试：[`tests/strongflow-role-authority.test.mjs`](../../tests/strongflow-role-authority.test.mjs)、[`tests/strongflow-role-isolation.test.mjs`](../../tests/strongflow-role-isolation.test.mjs)、[`tests/strongflow-dsh-approval.test.mjs`](../../tests/strongflow-dsh-approval.test.mjs)

## 结论

每个 StrongFlow 模型角色都使用一个受管 Codex Core 线程。线程启动前，TypeScript 根据不可修改的 `RoleSpec` 和权限矩阵生成 `GovernedSessionAuthority`；Rust 内核只接受与自己内置八角色表逐项相同的信封，并据此设置工作区、指令、推理强度、文件模式、人工决定策略和模型可见工具。调用方不能临时增加工具、改权限预设或换工作区。

线程启动后，Rust 重新读取 Codex 实际保留的配置并返回 `GovernedSessionEffectivePolicy`。TypeScript 再按当前角色逐项核对。缺字段、只执行部分限制、恢复时丢失工具或出现额外指令来源，都会关闭尚未接受的线程；会话不会进入活动列表，也不会写成已接受记录。

## 固定工具表

| 角色 | 模型可见工具 |
| --- | --- |
| Requirements、Solution、Planner | `artifact.read`、`artifact.write`、`workspace.read`、`code.search` |
| Reviewer、Verifier、Adversarial Verifier | 上述四项，加 `candidate.diff`、`command.run`、`test.run` |
| Executor、Remediator | 验证角色的七项，加 `candidate.patch` |

Codex 接收的是带命名空间的动态工具。DSH 模型桥只把这些工具转成提供商能接受的名称，例如 `candidate.patch` 显示为 `candidate__patch`；测试直接检查八个真实内核请求的完整名称集合，没有默认 Codex 命令、网页、MCP、子 Agent、计划或权限工具混入。

## 创建和恢复使用同一条路径

```text
RoleSpec + 固定权限矩阵
          │
          ▼
GovernedSessionAuthority
          │
          ▼
Codex 启动参数：工作区、指令、动态工具、空环境选择
          │
          ▼
Codex 实际配置快照与指令来源
          │
          ▼
GovernedSessionEffectivePolicy
          │
          ▼
StrongFlow 再核对 ──不同──▶ 关闭线程，不发布
          │相同
          ▼
安装事件处理器并发布会话
```

上游 Codex 原有 rollout 恢复入口会自行重建一部分启动选项，不能证明调用方给出的动态工具和空环境选择仍然存在。因此补丁 [`0004-resume-with-caller-options.patch`](../../upstream/patches/codex/0004-resume-with-caller-options.patch) 增加一个保留调用方 `StartThreadOptions` 的恢复方法。创建和恢复都由 WinWinCode 生成同一份权限信封；没有为旧会话保留第二条兼容路径。

## 工具调用

模型发出 `dynamic_tool_call_request` 后，宿主按以下顺序处理：

1. 核对事件的会话、事件流、上下文、调用编号和回合编号；
2. 把命名空间和工具名还原成固定工具标识；
3. 检查该角色的准确工具名单，名单外调用直接返回失败，执行器不会收到请求；
4. 严格检查参数形状，不接受多余字段；
5. 把现有读路径解析成分配根目录内的真实路径；新写路径逐段解析现有父目录，拒绝 `..`、绝对路径、反斜杠和跳出根目录的符号链接；
6. 只把经过检查的请求交给宿主工具执行器；
7. 用原始调用编号向 Codex 返回一次成功或失败结果。

路径检查用于在执行前拒绝明显越界，也会处理已经存在的符号链接。文件系统可能在检查后变化，因此最终打开文件和运行命令仍必须经过操作系统沙箱；这一层由 `winwincode-9c4.5.3` 完成，不能用本次路径检查代替。

## 人工决定

模型没有批准工具。Codex 自己产生 `exec_approval_request` 或 `apply_patch_approval_request` 时，宿主先记录请求，再通过 Cordis 的 `winwincode/strongflow/approval/request` 事件交给 DSH 界面。请求包含：

- 作业、阶段运行、尝试、角色和角色上下文；
- 操作类型和操作编号；
- 隐藏敏感字段并限制大小后的请求范围；
- Codex Core 权威标识、会话沿袭、实际会话、事件流、事件序号和回合编号。

界面只能返回 `approved`、`rejected`、`cancelled` 或 `unavailable`。没有回答者、回答值无效、回答者报错或会话取消都按未批准处理。宿主记录决定后，才按同一个操作编号把结果送回 Codex。记录或交互失败时会再次尝试发送拒绝并让角色会话失败，不会把异常解释为同意。

## 本任务和下一任务的边界

本任务已经固定并验证：线程启动前的角色权限、启动后的实际配置证据、准确模型工具、恢复保持调用方设置、工作区路径准入、名单外调用不执行、动态工具回传，以及带完整来源的 DSH 人工决定入口。

`winwincode-9c4.5.3` 继续负责真实宿主工具执行器的操作系统进程限制、网络隔离、凭据按引用解析、环境变量白名单、持久化脱敏审计和相关泄漏测试。只有下一任务也通过后，产品才可以声称完整宿主沙箱已经执行。
