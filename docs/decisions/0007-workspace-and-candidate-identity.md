# ADR-0007：每项作业使用隔离工作区和内容身份

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.4.1`
- 合同：[`packages/contracts/src/strongflow-workspace.ts`](../../packages/contracts/src/strongflow-workspace.ts)
- 规则实现：[`packages/strongflow/src/workspace-policy.ts`](../../packages/strongflow/src/workspace-policy.ts)

## 结论

WinWinCode 不让角色直接使用用户当前打开的源码目录。每项 StrongFlow 作业从一个干净、已确定的 Git 提交和树创建三类隔离目录：只读源码快照、唯一可写候选工作区和按候选创建的只读验证副本。

```text
用户源码仓库（不归 WinWinCode 清理）
                │ clean commit + tree
                ▼
HOME/strongflow-workspaces/SHA256(JobId)/
├── source/                 定义与规划角色只读
├── candidate/              Executor / Remediator 可写
├── verification/SHA256/    评审与验证角色只读、可重建
└── metadata/               后续生命周期记录
```

目录名使用摘要，不把 `JobId`、分支名或模型输出直接放进路径。实际创建、删除和 Git worktree 命令由后续生命周期实现完成；本决策固定它必须遵守的身份、目录、权限和清理规则。

## 源码准入

允许的源码状态只有两种：

1. 干净分支上的已解析 `HEAD`；
2. 干净的 detached HEAD。

两者都必须取得 Git 对象格式、提交号和树号。支持 Git SHA-1 的 40 位小写对象号和 Git SHA-256 的 64 位小写对象号。

其他状态有明确结果：

| 观察结果 | 处理 |
| --- | --- |
| 已跟踪、已暂存或未跟踪内容发生变化 | `DIRTY_SOURCE`，要求先提交或清理 |
| 索引冲突 | `AMBIGUOUS_SOURCE` |
| merge、rebase、cherry-pick、revert 或 bisect 正在进行 | `AMBIGUOUS_SOURCE` |
| unborn HEAD、缺少提交或树 | `AMBIGUOUS_SOURCE` |
| bare 仓库 | `UNSUPPORTED_SOURCE` |
| 相对路径、未知对象格式或对象号长度不匹配 | `INVALID_POLICY_INPUT` |

WinWinCode 不静默忽略脏内容，也不把未提交文件复制进候选。这样人看到的基线、Agent 实际使用的基线和后续差异审核使用的基线保持一致。

## 内容身份

所有派生身份使用带长度分隔的字段编码再计算 SHA-256，避免简单字符串拼接产生歧义。

### 源码快照

```text
SourceSnapshotId = SHA256(objectFormat, HEAD commit, HEAD tree)
```

仓库路径和分支显示名不进入内容身份。同一提交和树在不同 clone 中得到同一个 `SourceSnapshotId`；分支与 detached HEAD 指向同一内容时也得到同一身份。

### 候选

```text
CandidateId = SHA256(
  SourceSnapshotId,
  base commit,
  base tree,
  candidate commit,
  candidate tree,
  SHA256(exact diff bytes)
)
```

候选不使用随机号或界面会话号。基线、最终提交、最终树或差异字节中任一项变化都会产生新的 `CandidateId`。收到已有候选结构时，规则实现会重新计算源码和候选身份；字段被替换后不能继续冒充原候选。

### 验证副本

`VerificationSnapshotId` 绑定 `CandidateId`、候选提交和候选树。评审与验证角色必须同时收到完整候选身份和匹配的验证副本身份；只提供一个路径或不匹配的候选会被拒绝。实际目录还绑定角色和 `StageRunId`，所以同一候选上的 Reviewer、Verifier、Adversarial Verifier 以及同一角色的两次运行都不会共享可修改目录。

## 每个角色得到哪个目录

| 角色 | 模式 | 唯一目录 |
| --- | --- | --- |
| Requirements Analyst | `source-read-only` | `source/` |
| Solution Architect | `source-read-only` | `source/` |
| Planner | `source-read-only` | `source/` |
| Executor | `candidate-write` | `candidate/` |
| Remediator | `candidate-write` | `candidate/`，且必须引用当前候选 |
| Reviewer | `candidate-read-only` | 当前候选、角色和阶段运行独占的验证副本 |
| Verifier | `candidate-read-only` | 当前候选、角色和阶段运行独占的验证副本 |
| Adversarial Verifier | `candidate-read-only` | 当前候选、角色和阶段运行独占的验证副本 |

角色不能提交自选路径。传入的布局会按 `home`、`JobId` 和 `SourceSnapshotId` 重新生成并逐项核对，修改过的 candidate 路径或 verification 路径不会被接受。每次角色分配必须携带有效 `StageRunId`；验证目录和位于 Git worktree 外部的临时输出目录都由候选、角色和阶段运行共同计算。

## 路径与符号链接

供工具访问工作区内文件时只接受便携相对路径：

- 禁止绝对路径和 Windows 盘符；
- 禁止反斜杠；
- 禁止空段、`.`、`..`、控制字符和重复分隔符；
- 目标必须已经存在；
- 对根目录和目标执行 `realpath` 后，目标仍必须位于根目录内。

因此，工作区内指向外部目录的符号链接即使表面路径没有 `..`，也会得到 `SYMLINK_ESCAPE`。指向工作区内部的符号链接可以解析为内部真实路径。

## 唯一写入者

候选工作区同时最多存在一个写入租约。只有 `executor` 和 `remediator` 可以申请；租约绑定：

- `CandidateWriterLeaseId`；
- `JobId`；
- `StrongFlowWorkspaceId`；
- 角色；
- `StageRunId`；
- `AttemptId`；
- 取得时间。

完全相同的租约重放返回原租约，其他申请在原租约释放前得到 `WRITER_CONFLICT`。释放必须携带完全相同的租约身份；进程重启不能因内存清空而自动释放，后续生命周期实现需要把租约事实持久化并通过明确恢复流程处理。

## 清理与保留

| 内容 | 规则 |
| --- | --- |
| 用户原始仓库 | 不由工作区清理修改或删除 |
| `source/` | 保留到作业进入终态 |
| `candidate/` | 保留到作业终态且写入租约已释放 |
| 验证副本 | 对应只读运行结算后可删除，需要时从候选提交重建 |
| 制品、审核、差异和证据 | 不属于工作区清理范围，保存在持久制品存储中 |

删除验证副本不会删除正式审核依据。正式依据是候选身份、Git 对象、精确差异和持久证据；工作区只是可重建的执行目录。
