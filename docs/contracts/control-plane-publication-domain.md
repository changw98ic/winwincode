# Control Plane Publication Domain 与端口门禁

- 正式决定：[ADR-0028](../decisions/0028-control-plane-worker-migration.md)
- 机器规则：[control-plane-publication-domain.rules.json](./control-plane-publication-domain.rules.json)
- 对应任务：`winwincode-9c4.16.3.2`
- 状态：Publication 领域、持久记录和 provider 端口已实现；GitHub HTTP adapter 由阶段 3.3 的唯一实现接入

## 唯一发布路径

Publication 只走下面一条路径：

```text
current Delivered Delivery
  + current frozen candidate and exact Artifact digest
  + current passing Verdict
  + resolved exact human publication approval
  + immutable source issue and pull-request target
  → sealed PublicationAuthorization
  → PublicationCoordinator + PublicationLedger + PublicationPort
  → durable complete intent
  → branch → pull-request → issue-comment → commit-status
  → secret-safe PublicationResultFact
```

公开 publish command 只携带 `publicationId`、`deliveryId`、`candidateDigest` 和 `target`。
Verdict、人工批准、Artifact、源码身份和 provider operation 都不接受调用方 JSON。Control Plane
先从同一个当前 Delivery 截面得到 Delivered revision、Spec、当前 frozen candidate、Artifact
摘要、Pass Verdict、最新且唯一的人类发布批准、source 和 target，再交给 Publication crate
封存。缺一项、使用旧 candidate/Spec/Verdict/批准，或目标不一致，都会在创建 intent 和调用
provider 之前停止。

target 摘要直接使用 Delivery 当前 `GitHubPullRequestTargetRef` schema v3 的完整形状，包括
`schemaVersion` 和 `kind`，不会再用一份少字段的发布目标重算。发布批准者只接受 canonical
`usr_` 人类用户身份；service 或 system 身份不能代替这次人工批准。

Control Plane 之前临时拥有的 `PublicationFactBinding`、`PublicationResourceFact` 和
`PublicationResultFact` 已迁到 `winwincode-publication`，Control Plane 只使用这一份类型，
没有保留第二套发布事实。
公开投影中的 `publicationSetSha256` 直接来自这份已持久化授权，不能只对较小的
Delivery 绑定子集重新计算一个不同摘要。

## 先记录完整 intent，再处理外部系统

`PublicationLedger` 使用 canonical `ProductStateStorage`，一次提交同时保存 Publication 当前
状态、aggregate journal record、带 actor/scope/requestId 的 command receipt 和内部 outbox
事件。完整 intent 在任何 provider 调用之前写入；状态读取会逐条校验 manifest、journal
sequence、digest 和当前 state 是否等于 journal tail。

完全相同的 request 会先返回原 receipt，不读取当前 state、journal 或替换事实。同一 request
identity 改了正文会返回 request conflict；另一个 request 试图创建同一个 Publication 会返回
already exists。八个并发的相同 publish request 只形成一份 intent、一条初始 journal record、
一个 receipt 和一个 outbox 事件，随后也只执行一组外部操作，不会重复创建 PR。

## Provider 操作与恢复

`PublicationPort` 只接收 version 1、protocol
`winwincode.github-provider-operation.v1` 的闭合操作。每个操作都有稳定的 operation key、完整
request SHA-256 和固定 payload。Coordinator 严格按
`branch → pull-request → issue-comment → commit-status` 处理，并且每一步都先 lookup，再决定
是否 apply。

如果 provider 已写入但响应丢失，步骤会记录为 unknown。重启后 Coordinator 用同一个 key 和
request digest 再 lookup，找到原结果后继续后续步骤，不重复执行已完成写入。provider 明确
拒绝时 Publication 进入 failed，后续操作不再执行；已经成功的步骤不会被改回未完成。
结果只保存 PR 的 kind、repository、number 和经过核对的状态，不保存 credential、token 或
provider 原始错误正文。

## Cancel 与 Delivery 的边界

Cancel 需要当前 Publication revision，并用自己的 request receipt 支持精确重放。它只改变
Publication 状态和取消原因，停止后续 provider 调用；Cancel 只改变 Publication，不修改
Delivery 的 Verdict、candidate、Artifact 或人工批准。发布成功同样只增加
`PublicationResultFact`，不会把 Delivery Verdict 改写成另一种状态。

## 本阶段完成范围

Rust 黑盒测试实际使用 SQLite `PublicationLedger` 和 fake `PublicationPort`，覆盖 intent 原子
提交、并发重放、部分成功后重启恢复、provider 拒绝、状态与 journal 损坏、取消和
receipt-first。Control Plane 测试继续证明只有当前 Delivered Delivery、当前 candidate、Pass
Verdict、精确人类批准和 target 才能形成 publication binding。

Fake port 只证明 domain 与恢复规则，不代替生产 provider 测试。GitHub HTTP 与 credential reference adapter 已由阶段 3.3 的
[GitHub Publication Adapter 门禁](./control-plane-github-publication-adapter.md) 接入；它继续复用本页的
同一个 `PublicationPort`、durable intent 和恢复流程，没有建立第二套发布状态。
