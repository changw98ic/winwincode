# ADR-0010：工作区进程锁可核验、崩溃后只回收已死亡持有者

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.4.4`
- 实现：[`packages/strongflow/src/git-workspace.ts`](../../packages/strongflow/src/git-workspace.ts)
- 进程测试：[`tests/strongflow-git-workspace.test.mjs`](../../tests/strongflow-git-workspace.test.mjs)

## 结论

创建候选写入租约、冻结候选、创建或清理验证副本以及清理整个作业时，共用一个可核验的短时操作锁。锁不再是无法判断来源的空目录，而是一个已经写完所有者记录后再原子改名到固定位置的目录。

```text
metadata/.writer-operation-TOKEN.pending/
└── owner.json
    ├── JobId / WorkspaceId
    ├── owner token
    ├── process id
    └── acquired time
              │ atomic rename
              ▼
metadata/writer-operation.lock/
└── owner.json
```

准备目录带随机 token，写完并同步 `owner.json` 后，才尝试改名为唯一的 `writer-operation.lock`。并发进程只能有一个改名成功；其他进程等待当前锁释放。释放时也先把整个锁目录原子移到自己的 token 路径，再删除，因此不会误删刚由另一个进程取得的新锁。

## 崩溃判断

`StrongFlowGitWorkspaceManager.reconcile(JobId)` 重新计算唯一作业目录，核对 `owner.json`、源码仓库真实路径和 Git common directory，然后检查操作锁中的进程号：

- 进程仍存在或系统不允许探测时，返回 `operation-active`，不移动锁；
- 进程明确不存在时，把该锁目录原子移到唯一回收路径，复核所有者记录后删除，并标记为 `reclaimed`；
- PID 已被复用时会保守地视为仍在运行，因此可能需要稍后或人工处理，但不会回收另一个活进程的锁；
- 锁文件、所有者记录或路径被替换时停止处理，不猜测所有权。

回收死锁后，管理器把工作区归为四种明确状态：

1. `absent`：作业目录不存在；
2. `ready`：manifest 及 source/candidate worktree 都属于登记仓库；
3. `operation-active`：仍有活进程持锁；
4. `cleanup-required`：已确认是本程序所有，但创建或清理只完成了一部分。

`cleanup-required` 不会直接恢复执行。上层启动恢复流程可以先记录决定，再调用同一个 `dispose(JobId)` 完成可重复清理。候选冻结被杀时，如果两个 worktree 仍完整，则状态是 `ready`；重新冻结会从当前候选文件状态生成确定身份。

## 边界与并发验证

进程级测试覆盖以下事实：

- 同一 JobId 并发创建时只有一个完整工作区；
- 两个管理器并发申请写入租约时只有一个成功，重复申请保持原取得时间；
- Reviewer 和 Verifier 并发创建各自的 detached 副本，路径和临时输出互不相同；
- 精确嵌套 Git 仓库可以作为独立源码，父仓库和嵌套仓库都保持不变；
- candidate 路径被符号链接替换时，清理停止且不接触链接目标；
- 子进程分别在创建、冻结和清理中被 `SIGKILL` 后，新的管理器能识别保留目录、只回收已死亡的锁，并完成冻结或重复安全清理；
- 所有场景都再次比较原始源码文件、状态、提交和树。

完整作业事件、内核会话、人工审核和 UI 投影的联合恢复仍由后续启动恢复流程负责；本决定只提供可证明的工作区所有权和崩溃后状态。
