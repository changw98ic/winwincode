# ADR-0008：由作业独占 Git worktree 生命周期

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.4.2`
- 身份与路径规则：[`packages/strongflow/src/workspace-policy.ts`](../../packages/strongflow/src/workspace-policy.ts)
- 生命周期实现：[`packages/strongflow/src/git-workspace.ts`](../../packages/strongflow/src/git-workspace.ts)

## 结论

每项 StrongFlow 作业使用两个由 WinWinCode 创建和登记的 detached Git worktree：`source/` 是定义、方案和规划阶段使用的源码快照，`candidate/` 是 Executor 或 Remediator 使用的候选目录。两者从同一个已解析提交创建，用户原始检出目录不作为 Agent 工作目录。

```text
干净的用户检出目录
        │ 解析 commit / tree，但不切换分支、不写文件
        ▼
HOME/strongflow-workspaces/SHA256(JobId)/
├── source/       detached，只读角色使用
├── candidate/    detached，持有写入租约的角色使用
└── metadata/
    ├── owner.json       创建 worktree 前写入的所有权标记
    ├── manifest.json    两个 worktree 验证完成后发布
    ├── writer.json      当前唯一写入租约
    ├── writer-operation.lock/ 当前短操作的进程所有者记录
    └── failure.json     部分创建失败时保留的诊断结果
```

作业目录只能由 `home` 和 `JobId` 通过同一个 `strongFlowWorkspaceRootForJob` 函数计算。创建、打开和清理不接受调用方提交的目录，避免“创建时用一个算法、清理时用另一个算法”导致删错位置。

## 创建前检查

创建任何作业目录前，生命周期管理器先完成以下检查：

1. 传入路径必须真实存在，并且正好是 Git worktree 顶层目录；
2. 仓库不是 bare 仓库，Git 对象格式为 SHA-1 或 SHA-256；
3. 原始检出目录没有已跟踪、已暂存或未跟踪变化，也没有冲突或未完成的 Git 操作；
4. 请求的 revision 能解析为一个确定的提交和树；
5. 记录原始检出目录的状态、`HEAD` 提交和树，用于创建后的再次核对。

仓库、revision 或源码状态不合格时，不创建该 `JobId` 的目录，也不启动 Agent。指定旧提交作为基线时，原始分支仍停留在当前提交；只有新建的两个 worktree 指向所选基线。

## 创建与失败保留

管理器先以独占目录创建作业根，再写入 `owner.json`，然后依次运行两个有超时和输出上限的 `git worktree add --detach`。每个新 worktree 都要核对：

- 顶层真实路径就是预定路径；
- Git common directory 与原始仓库一致；
- 提交和树与已选源码身份一致；
- 初始状态干净。

两个 worktree 都通过后才写 `manifest.json`。如果 `owner.json` 之后任一步失败，目录不会被静默删除；`failure.json` 记录失败类型，错误同时返回保留目录。这样恢复流程可以检查并清理由本程序确实创建的部分 worktree。

Git 子进程不经过 shell，不接受交互式凭据，超过时间或输出上限会被终止。对应结果分别是 `GIT_COMMAND_TIMEOUT` 和 `GIT_OUTPUT_LIMIT`。

## 唯一写入租约

`writer.json` 使用“先完整写临时文件，再建立目标硬链接”的方式发布，因此其他进程只会看到完整租约，不会读到半个 JSON 文件。

申请、释放和清理共用 `writer-operation.lock` 作为跨进程短锁。锁目录在随机准备路径中写完进程所有者记录，再原子改名到固定位置：

- 同一时间只有一个申请或释放动作可以改变租约；
- 完全相同的申请重试返回原租约和原取得时间；
- 其他 Executor 或 Remediator 在租约释放前得到 `WRITER_CONFLICT`；
- 活跃租约存在时，整个作业目录不能清理；
- 短锁超过规定时间仍未释放时，保留现场并返回 `WRITER_OPERATION_TIMEOUT`；启动恢复只能在操作锁记录的进程明确死亡后原子移走该锁。

这道短锁只保护租约文件的变化。角色能否写候选目录仍由固定角色配置、工作区分配和后续内核工具范围共同执行。

锁所有权、死亡进程回收和中断创建/冻结/清理的状态分类详见 [ADR-0010](0010-workspace-crash-reconciliation.md)。

## 检查与清理

运行期间的检查会重新读取两个 worktree。`source/` 的提交、树或文件状态发生任何变化都会得到 `SOURCE_SNAPSHOT_MUTATED`；`candidate/` 的变化则作为候选状态返回，供后续冻结和差异生成使用。

清理只接受 `JobId`，并按以下顺序执行：

1. 重新计算唯一作业根；
2. 验证根目录、`owner.json`、源码仓库真实路径和 Git common directory；
3. 在短锁内确认没有活跃写入租约；
4. 先检查所有现存 worktree 都位于预定路径且属于登记仓库；
5. 所有检查通过后，才逐个运行 `git worktree remove --force`；
6. 清理 Git 登记并删除这个已确认归属的作业根。

检查和删除分成两段，避免先删掉正常的 `source/`，然后才发现 `candidate/` 已被替换。符号链接、被替换的目录、错误的所有权标记或属于其他仓库的 worktree 都会中止清理。目标已经清理时再次调用返回 `absent`，不会影响用户仓库或其他作业。

## 已验证行为

进程级测试使用临时 Git 仓库确认：

- 两项作业得到不同目录，两个 worktree 都是 detached；
- 原始文件字节、Git 状态、分支提交和树在创建、候选修改及清理后保持不变；
- 显式旧 revision 不会移动原始分支；
- 非仓库、缺失 revision 和脏源码在创建目录前失败；
- 源码快照被修改可以检测，候选修改可以读取；
- 两个管理器并发申请时只有一个写入者，重试不会改变取得时间；
- 活跃写入者阻止清理，释放后可以清理且重复调用安全；
- worktree 被符号链接替换时，清理不删除链接目标，也不先删除另一个正常 worktree；
- 第二个 worktree 创建失败时，所有权和失败记录保留，并能通过同一安全清理流程回收。
