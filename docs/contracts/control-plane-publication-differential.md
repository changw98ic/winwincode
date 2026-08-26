# Publication、Artifact、Audit 与 Policy 切换门禁

- 正式决定：[ADR-0028](../decisions/0028-control-plane-worker-migration.md)
- 机器规则：[control-plane-publication-differential.rules.json](./control-plane-publication-differential.rules.json)
- 对应任务：`winwincode-9c4.16.3.7`
- 状态：Rust Control Plane 已成为唯一 Publication 写入者

## 唯一运行路径

旧 TypeScript 写入入口已经删除。StrongFlow 仍保留人工批准的界面逻辑，但不会生成审核包、保存
Publication journal、读取 GitHub 凭据或调用 GitHub。当前唯一写入路径是：

```text
Delivery → review package Artifact → policy/audit → GitHub
```

具体顺序如下：

```text
current Delivery + frozen candidate
  → Control Plane publication preparation
  → content-addressed ArtifactStore review package
  → generated publication.publish
  → repository policy + immutable decision audit
  → SQLite Publication intent
  → policy-guarded resume
  → GitHub PublicationPort
```

审核包只包含当前 Delivery 的安全投影、目标、候选 Git 关系、Artifact 引用和批准身份/时间。它不包含
WorkerSession、CodexThread、原始 Attention 文本、日志、提示词、lease、fence、token 或凭据。相同内容
会得到相同 Artifact 标识；未获批准的 Delivery 在创建 Artifact 前就被拒绝。

## GitHub 顺序与失败恢复

远端顺序固定为：

```text
branch → pull-request → issue-comment → commit-status
```

每一步先 lookup，再在明确不存在时 apply。完整门禁使用真实 SQLite、真实本地 ArtifactStore 和
loopback HTTP GitHub 假服务。测试会让 issue-comment 已经写入远端、但故意丢掉 HTTP 响应；第一次
resume 保持 Publishing，并写入 `publication.incomplete`，失败不会被记录成 Published。重启 Control
Plane 后，第二次 resume 先找到已有 comment，不重复写入，再完成 commit status，最终写入
`publication.published` 并保留精确 PR identity。

| 样本 | 当前结果 | 具体含义 |
| --- | --- | --- |
| success/restart | Published revision 12 | 同一个审核包 Artifact 和同一个 PR 在重启后继续使用；远端 comment 不重复。 |
| authentication failure | `github-permission-denied` | 凭据或权限拒绝是终态；后续远端步骤不调用，原始诊断和 token 不保存。 |
| rate limit | `github-rate-limited` | 限流保持可恢复；下一次 resume 重新 lookup，而不是猜测上次是否成功。 |
| PR conflict | 精确已有 PR 被接管 | 只有 marker、目标、请求摘要和 resource identity 全部相等时才接受竞态结果。 |
| comment rejection | Failed，保留已确认 PR | comment rejection 不会继续写 commit status，也不会伪装发布成功。 |
| Artifact object corruption | `ArtifactErrorKind::Corrupt` | 返回任何 bytes 或调用 GitHub 前重新计算摘要，损坏内容不会进入发布。 |
| policy denied | `PERMISSION_DENIED` | 只有拒绝审计，没有 Publication intent、Artifact 或 GitHub 调用。 |
| duplicate command | 返回原 receipt | actor、完整 repository scope、requestId 和 payload digest 相等时不增加审计或副作用。 |

## Audit 与最终状态

每次新的 publish 或 resume 都先写 immutable policy decision audit。命令或恢复完成后，再写一条
绑定 actor、完整 scope、requestId、Delivery、Publication revision 和 Publication digest 的结果审计：

- Pending intent：`publication.intent-recorded`；
- 尚未确认全部外部结果：`publication.incomplete`；
- 已完成全部远端步骤：`publication.published`；
- 终态失败或取消：`publication.failed` / `publication.cancelled`。

结果事件使用稳定身份；相同 receipt 重放不会增加审计。Publishing、Published、Failed 和安全错误码
都从唯一 SQLite Publication journal 恢复，不从 GitHub 响应文本重建。

## 自动验证与切换结果

机器门禁实际运行四组 Rust 黑盒证据：

1. Delivery → Artifact → policy/audit → loopback GitHub 的完整失败恢复；
2. Control Plane policy、receipt、结果审计和重启；
3. GitHub adapter 的认证、限流、PR conflict、comment rejection；
4. ArtifactStore 的内容损坏检测。

门禁还会逐个确认旧 TypeScript writer、review-package writer、DSH GitHub provider、package export、
Cordis row 和对应测试均已删除。Rust 不读取旧 DTO、旧 journal 或旧错误码，也没有 fallback 或双写。
阶段 3 的 Publication、Artifact、Audit 与 Policy 已切换完成，阶段 4 可以开始。
