# Control Plane GitHub Publication Adapter 门禁

- 正式决定：[ADR-0028](../decisions/0028-control-plane-worker-migration.md)
- Publication 领域规则：[control-plane-publication-domain.rules.json](./control-plane-publication-domain.rules.json)
- 本阶段机器规则：[control-plane-github-publication-adapter.rules.json](./control-plane-github-publication-adapter.rules.json)
- 对应任务：`winwincode-9c4.16.3.3`
- 状态：GitHub credential reference、REST adapter、失败分类与持久恢复已实现并由真实 Rust 测试执行

## 唯一运行路径

GitHub 发布只走下面一条路径：

```text
current sealed PublicationAuthorization
  → durable Publication intent and closed provider operations
  → GitHubPublicationAdapter + PublicationCoordinator + PublicationLedger
  → branch → pull-request → issue-comment → commit-status
  → secret-safe PublicationResultFact
```

`GitHubPublicationAdapter` 直接实现既有 `PublicationPort`。它不建立第二套发布状态机，
也不复制 Coordinator 已经持久化的进度。source Issue 使用
`PublicationSourceIssue`；review package 已封存在 pull-request body 中。Release 尚未进入 canonical v1 operation protocol，
因此这里没有临时第五步或旧实现回退。

## Credential reference 边界

adapter 配置只保存 canonical `CredentialReferenceId`。它在每次 HTTP 请求之前调用
Control Plane 提供的 `GitHubCredentialResolver`，取得只存在于本次请求内存中的 GitHub
credential。解析结果必须声明 provider 为 `github`；缺失、拒绝、暂时不可用和 provider
不一致分别返回稳定的安全错误码。

`GitHubCredential` 不能序列化、反序列化或 clone，`Debug` 永远显示
`[REDACTED]`。token 不进入 `PublicationOperation`、Publication state、journal、receipt、
outbox、`PublicationResultFact` 或错误。adapter 也不会把 GitHub 返回的原始错误正文写入这些
位置。

## GitHub HTTP 与精确核对

远端 base URL 必须使用 HTTPS；普通 HTTP 只允许 loopback 测试服务器。base URL 不接受
userinfo 或 query，客户端关闭 redirect，使用固定 GitHub API version `2022-11-28`、30 秒
总超时、2 MiB 响应上限和最多 100 页的有界 lookup。

每个操作都先 lookup，再决定是否 apply：

- branch 精确核对 repository、branch 与 commit SHA；
- pull request 精确核对 base/head repository、branch、title、完整 body 与稳定 operation marker；
- issue comment 精确核对 source repository、issue number、完整 body 与 operation marker；
- commit status 精确核对 repository、commit、context、state、description 与 source Issue URL。

PR 的 durable resource 只保留 `kind + repository + number`，不保存 GitHub HTML URL。GitHub
创建返回 409 或 422 时，adapter 只做一次 exact lookup；找到完整相同对象才算已完成，字段
冲突或查不到都不会伪报成功。相同 head/base 上已经关闭、且没有当前 operation marker 的历史
PR 不拥有本次请求；仍处于 open 的无 marker PR 会明确形成 route conflict。
同一组织内的跨仓库 PR 使用 `owner:branch` 和 GitHub 要求的 `head_repo` repository name，
同时仍在 lookup 响应中精确核对完整 head repository，避免把同 owner 的另一个 fork 当成本次 PR。

## 失败与重启恢复

HTTP 401 分类为 `github-authentication-failed`，普通 403 分类为
`github-permission-denied`，429 或带 rate-limit 证据的 403 分类为
`github-rate-limited`，5xx 分类为 `github-service-unavailable`，传输中断分类为
`github-transport-unknown`。远端 diagnostic 正文不进入稳定错误码。

rate limit、5xx 和响应丢失形成 durable unknown，Publication 保持 publishing。重启后
Coordinator 从 SQLite 中原来的 operation key、request SHA-256 和 closed payload 继续 lookup，
不会重新生成请求，也不会重复创建已经存在的 GitHub 对象。权限等明确拒绝会让 Publication
进入 failed；这同样适用于 lookup 阶段已经明确得到的 authentication 或 permission 拒绝。例如
PR 被拒后不会调用 comment 或 status。已经成功的 branch 或 PR 仍作为真实的部分进度保存，
但后续步骤不能反过来把失败的 PR 记录成成功；comment 被拒时也会保留已确认的 PR number，
同时跳过 status。

## 测试证据

默认门禁使用真实 loopback HTTP server、真实 `GitHubPublicationAdapter`、
`PublicationCoordinator` 和 SQLite `PublicationLedger`。它覆盖完整四步创建与 lookup、逐请求
credential resolution、重复 PR 竞争、写入成功但响应丢失后的重启恢复、permission、rate
limit、后续步骤停止，以及 SQLite 文件中没有 token。

可选真实仓库 lane 只有在显式设置 `WINWINCODE_GITHUB_LIVE_TEST=1` 和完整仓库、commit、Issue、
base/head branch、token 环境变量时才运行。它对同一组 operation 先 lookup/apply，再次 lookup
确认收敛；普通本地与 CI 门禁不会意外修改 GitHub。
