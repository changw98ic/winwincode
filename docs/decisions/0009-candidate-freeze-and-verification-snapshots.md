# ADR-0009：候选先冻结，再创建独立验证副本

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.4.3`
- 内容身份：[`packages/strongflow/src/workspace-policy.ts`](../../packages/strongflow/src/workspace-policy.ts)
- Git 实现：[`packages/strongflow/src/git-workspace.ts`](../../packages/strongflow/src/git-workspace.ts)

## 结论

Executor 或 Remediator 结束并释放唯一写入租约后，WinWinCode 把候选目录的完整文件状态变成一个确定的 Git 提交、树和精确差异。Reviewer、Verifier 和 Adversarial Verifier 不能直接打开这个可交付候选；每个角色的每次阶段运行都从冻结提交创建自己的 disposable Git worktree。

```text
candidate/（唯一写入者结束）
       │ stage all + scope check
       │ deterministic commit/tree + exact binary diff
       ▼
CandidateRecord
├── base commit / tree
├── candidate commit / tree
├── SHA256(exact diff bytes)
├── changed paths + approved scope
└── immutable diff file
       │
       ├── reviewer + StageRunId A ── verification worktree A
       ├── verifier + StageRunId B ── verification worktree B
       └── adversarial + StageRunId C ─ verification worktree C
```

## 冻结条件

冻结动作与写入租约申请、释放和整个作业清理共用同一把短锁。以下任一情况都会停止冻结：

- Executor 或 Remediator 仍持有写入租约；
- 只读源码快照的提交、树或文件状态发生变化；
- candidate 不再是登记仓库的预定 worktree；
- Git 索引存在未解决冲突；
- 冻结期间文件再次变化；
- 变化路径超出本次批准范围。

范围有两种明确模式：`repository` 允许仓库内所有便携路径，`paths` 只允许列出的文件或目录根。路径必须是有效 UTF-8、不能是绝对路径、不能包含反斜杠、控制字符、空段、`.` 或 `..`。Git 返回的每个新增、修改或删除路径都要逐项核对。

## 候选身份与提交

管理器把 candidate 的最终工作目录状态加入索引，生成树，并从最初源码提交计算以下固定格式的 diff：

- binary patch；
- full index object IDs；
- no color、no external diff、no text conversion；
- no rename inference；
- 固定 `a/` 和 `b/` 前缀。

候选提交通过 `git commit-tree` 生成，父提交始终是源码基线。作者、提交者、时间和消息格式固定，因此同一个基线、树和 diff 会得到同一个 Git 提交。没有任何变化时直接使用基线提交，不制造空提交。

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

冻结前后会再次比较索引、树和差异。确认一致后，candidate worktree 被重置到这个提交并保持干净。元数据目录保存不可覆盖的候选记录和原始 diff 字节，`current-candidate.json` 只指向当前冻结结果。相同内容和相同范围的重复冻结返回同一记录；同一身份对应不同内容或范围会被视为损坏。

## 旧冻结何时失效

以下任一事实都会让旧冻结不能再用于新审核：

1. 新的 Executor 或 Remediator 已取得写入租约；
2. candidate worktree 变脏；
3. candidate 的 `HEAD` 提交或树不再匹配记录；
4. 精确 diff 文件或当前候选记录不匹配；
5. 新冻结已经替换 `current-candidate.json`。

旧的候选记录和 diff 不被覆盖，它们仍可用于解释历史结果和安全清理历史验证副本；但创建或重新打开审核目录必须引用当前冻结候选。

## 独立验证副本

每个验证目录由以下四项共同确定：

- `VerificationSnapshotId`；
- Reviewer、Verifier 或 Adversarial Verifier 角色；
- `StageRunId`；
- 作业布局。

因此两个角色、同一角色的两次运行、并行验证都得到不同路径。创建时使用 `git worktree add --detach CANDIDATE_COMMIT`，随后核对 common Git directory、提交、树和干净状态。验证副本的 `HEAD` 和提交树必须始终指向冻结候选；角色在副本中的临时修改不会进入 candidate，也不会被合并。

每个运行另有一个位于 Git worktree 外的 `verification-output/` 目录，供构建日志、缓存和临时结果使用。后续角色启动器应把可配置的构建输出优先指向这里，减少验证副本中的无关变化。

## 清理规则

验证副本有独立所有权和完成记录。清理按候选、角色和 `StageRunId` 重新计算路径，验证所有权与 Git common directory 后运行 `git worktree remove --force`，再删除对应临时输出和元数据。重复清理返回 `absent`。

只要任何验证目录或临时输出仍存在，整个作业目录就返回 `VERIFICATION_ACTIVE`，不会先删除 source 或 candidate。验证目录内的修改全部丢弃；候选提交、候选记录和精确 diff 保持不变。

## 已验证行为

进程级 Git 测试确认：

- 活跃写入者不能冻结，释放后可以冻结；
- 候选记录包含基线、候选提交、候选树、diff 身份、精确 diff 文件和变化路径；
- 相同候选重复冻结得到相同身份；
- 超出批准路径的变化被拒绝；
- 不同角色和不同阶段运行得到不同 detached worktree 和不同临时输出目录；
- 修改 Reviewer 副本不会改变 candidate 或 Verifier 副本；
- 新写入租约、candidate 变化和新冻结都会阻止旧候选进入新审核；
- 活跃验证副本阻止整个作业清理，逐个清理可重复且不影响候选。
