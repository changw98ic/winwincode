# Control Plane Publication 与 Repository Policy

- 正式决定：[ADR-0028](../decisions/0028-control-plane-worker-migration.md)
- Publication 领域规则：[control-plane-publication-domain.rules.json](./control-plane-publication-domain.rules.json)
- Audit Store 规则：[control-plane-audit-store.rules.json](./control-plane-audit-store.rules.json)
- 本阶段机器规则：[control-plane-publication-policy.rules.json](./control-plane-publication-policy.rules.json)
- 对应任务：`winwincode-9c4.16.3.5`
- 状态：第一组 Repository Publication Policy 已实现，并由真实 Control Plane、SQLite 产品存储、SQLite AuditStore 和 fake provider 黑盒测试执行

## 唯一路径

Publication 发布只走下面一条路径：

```text
generated `publication.publish`
  → authenticated requester + exact RepositoryScope
  → current sealed PublicationAuthorization + PublicationPolicyEvidence
  → deterministic RepositoryPublicationPolicy
  → immutable policy allow/deny audit
  → durable Publication intent
  → policy-guarded resume → PublicationPort
```

Control Plane 把 generated `publication.publish` 映射到既有 Publication domain command。
command 只带 publicationId、deliveryId、candidateDigest 和 target；当前 Delivery、candidate、
Artifact、human approval 和 repository scope 仍来自 sealed `PublicationAuthorization`，不会从
调用方 payload 中复制一套权威事实。policy evidence 再把独立验证、Artifact 可外发状态和观察
时间绑定到同一个 publication-set digest 与 repository-scope digest。repository-scope digest
必须等于精确 organization、workspace、project、repository 组成的 canonical
`RepositoryPolicyScope` JSON 的 SHA-256；只复制另一份 authority digest、即使 GitHub repository
相同也不能通过。foreign、过期或不匹配的 facts 在创建 intent 前停止。

## 固定策略和拒绝顺序

`RepositoryPublicationPolicy` 绑定 organization、workspace、project、repository 和 GitHub
repository。它只接受 User、ServiceAccount、System 三种 requester；requester 和 approver
分别有 allow 集合与 explicit deny 集合；仓库写入、Artifact 外发也只有 allow/deny 两种值。
此外，它可以要求独立验证，并给 human approval 设置最大有效时间。

规则按下面顺序求值，显式 deny 固定优先于 allow：

1. requester explicit deny；
2. approver explicit deny；
3. repository write deny；
4. Artifact export policy deny；
5. requester 不在 allow 集合；
6. approver 不在 allow 集合；
7. 缺少所需独立验证；
8. 当前 Artifact 不可外发；
9. approval 已过期；
10. allow。

policy 会先把成员排序并拒绝重复值，再对完整 closed policy 计算稳定 SHA-256。decision 只保存
allow/deny、稳定 ruleId、policy digest、requester、精确 scope、requestId、origin、Delivery、
Publication、时间和 decision digest；它没有 raw policy text、credential、prompt、provider
diagnostic 或任意 payload 字段。

## 审计先于业务和外部副作用

`PublicationCoordinator` 构造时必须接收 `PublicationPolicyAudit`。每个新的 publish 或 resume
都先求出唯一 decision，再先写入不可变 AuditStore。allow 使用 `policy.allowed`，deny 使用
`policy.denied`；Audit action name 是命中的 ruleId，unchanged state digest 绑定 policy digest。
Audit event 同时保留 actor、完整 repository scope、origin、requestId、Delivery 和 Publication。

记录失败时返回 `SERVICE_UNAVAILABLE`，并在任何 Publication intent 之前停止。deny 成功记录后
返回 `PERMISSION_DENIED`，同样不写 intent。resume 也在任何 provider lookup 或 apply 之前重新
读取当前 policy facts 并写 decision；deny 或 AuditStore 不可用时既不推进 Publication revision，
也不接触 GitHub。

唯一例外是已经提交成功的同一 publish command 的 exact receipt replay。它在读取 current policy
facts、current state/journal 和 audit 之前直接返回原 Publication，所以 policy 后来变化、当前
事实损坏或 AuditStore 暂时关闭都不会改变第一次已经提交的结果，也不会增加第二条审计记录。
同 requestId 改 body 仍是 idempotency conflict；不同 request 创建相同 Publication 仍是
already-exists/wrong-state，不会借 replay 绕过 policy。

## UI 可展示的错误

policy deny 映射为不可重试的 `PERMISSION_DENIED`。details 固定给出 `ruleId`、`repositoryId`
和 `publicationId`，UI 可以直接说明是哪条规则阻止了哪个仓库的哪次发布。AuditStore 不可用
映射为可重试的 `SERVICE_UNAVAILABLE`；它不把存储正文或内部诊断暴露给 UI。

stale/foreign trusted facts、request conflict、revision conflict、missing Publication 和 provider
失败仍使用 canonical generated error code，不建立 Policy 专用的第二套错误 envelope。

## GitHub 与 Worker 边界

`GitHubPublicationAdapter` 只实现 `PublicationPort`，负责 exact provider lookup/apply 和
credential reference；它没有 `PublicationLedger`，也不能构造 `PublicationCoordinator`、写
Publication intent 或跳过 policy audit。生产代码中 Coordinator 只由 Control Plane 的 policy
模块构造。测试用 wrapper 受 `test-support` feature 限制，并且也使用同一 policy/audit 路径。

recovery Worker 只能调用 Control Plane 的 typed Rust `resume_publication` application seam，不能
直接调用 GitHub adapter 改写 Publication。阶段 3.5 不新增 HTTP resume 或 Audit query；当前
canonical HTTP schema 仍只有既有 publish/cancel command，后续 transport 切换不能复制第二条
Publication 或 Policy 路径。

## 自动验证

机器门禁执行真实 `winwincode-control-plane` integration test，而不是只查测试名。测试使用：

- `ControlPlane::start_local` 和真实 SQLite product state；
- 同一 local root 中独立的 immutable SQLite AuditStore；
- 只计数 lookup/apply 的 fake `PublicationPort`；
- generated `PublicationPublishCommand`；
- 重启、关闭 AuditStore、再次打开相同 durable root 的完整流程。

它证明 allow 在 intent 前留下唯一审计；requester explicit deny 在多条拒绝同时命中时优先；
repository write、独立验证、Artifact policy、Artifact current fact 和 approval age 分别拒绝且零
provider 调用；AuditStore 缺失时新 intent fail closed；exact receipt replay 不重新求值；resume
在 provider 前完成 audit。
