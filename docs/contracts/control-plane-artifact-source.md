# Control Plane Artifact 与源码身份门禁

- 正式决定：[ADR-0028](../decisions/0028-control-plane-worker-migration.md)
- 机器规则：[control-plane-artifact-source.rules.json](./control-plane-artifact-source.rules.json)
- 对应任务：`winwincode-9c4.16.3.1`
- 状态：已实现并由 Rust 黑盒测试执行

## 唯一事实链

Artifact 和 Git 源码身份只走下面一条路径：

```text
generated artifact.open / artifact.chunk
  → durable ExecutionJob + RepositoryScope
  → sealed SessionBindingAuthority
  → ArtifactStore metadata catalog
  → ArtifactObjectStore bytes
  → GitSourceResolver rebuilds source identity
  → opaque ValidatedGitSourceArtifact
  → exact settled successful Worker outcome
  → FrozenDeliveryCandidate
```

Worker 只能在 `artifact.open` 里声明 Artifact 的内容摘要、长度、类型和文件显示名，再按
sequence 发送 `artifact.chunk`。Control Plane 先核对持久化的 ExecutionJob、完整仓库
scope、StageRun、SessionBinding、WorkerSession、Lease、fencing token 和 Worker 实例，
然后才把消息交给 ArtifactStore。ArtifactId、messageId 和 requestId 都是持久身份；同一
身份的逐值重放返回 duplicate，改动消息内容则返回 conflict。过期 Lease、旧 fencing token
和已替换 Worker 实例返回生成合同中的明确拒绝状态，不会预留 metadata 或写入 chunk。

每个 chunk 继续绑定 Artifact 首次打开时的完整 ExecutionJob provenance、发送时间和
content type。相同仓库里的另一个 Job 也不能接续上传。sequence 缺口返回可恢复的
`replayFromSequence`；chunk 摘要或最终聚合摘要不符返回
`ARTIFACT_DIGEST_MISMATCH`。成功 ACK 只表示 metadata 与对象字节都已按这条身份链接受。

## Metadata 与对象字节

`ArtifactStore` 是唯一 Artifact 存储接口。SQLite catalog 只保存不可变 metadata、scope、
来源、retention、连续 sequence 和删除墓碑；大对象交给 `ArtifactObjectStore`。对象键只由
SHA-256 内容摘要得到，调用方不能传本地路径、对象存储键或上传 URL。

本地 adapter 在受控根目录中使用先写完并 `fsync`、再 hard-link 的方式发布 chunk 和最终
对象，避免并发读取半个文件。两个并发的相同 chunk 收敛为一次新写和一次 duplicate；部分
上传关闭并重启后从持久 sequence 继续。Fake adapter 执行同一合同，用于证明将来的企业
对象存储 adapter 不能改变产品结果；它不是企业对象存储已经交付的声明。

Artifact 完成前不能读取或走 retention 删除。读取同时核对 scope、ArtifactId、内容摘要和
完整 ExecutionJob provenance，并在返回 bytes 前重算大小与 SHA-256。对象丢失或内容变化
均按 corruption 拒绝。删除先保留不可复用的 ArtifactId 墓碑；retention 未到期或 indefinite
时拒绝。多个 Artifact 共用同一内容地址时，物理删除与新 metadata 引用在 SQLite 写锁下
串行化，最后一个可删除引用消失前不会删除对象。

## Git 源码重建

candidate Artifact 的正文是严格、闭合且字节级 canonical 的 v1 JSON，只含一个
`candidateCommitId` 提示。它不含 tree、diff、changed path、objectId 或 hunk hash。

`LocalGitSourceResolver` 只打开配置根目录下的 canonical repository。每条 Git 命令清空
继承环境，再固定关闭全局/系统配置、replace refs、可选锁、external diff 和 textconv。
resolver 从受控仓库重新计算并逐项封存：

- pinned base commit 与 base tree；
- candidate commit 与 candidate tree；
- candidate 必须是 base 的后继；
- binary/full-index diff 的 SHA-256；
- 排序后的 UTF-8 portable changed paths；
- 每个 present path 的 Git object ID，deleted path 的空 object；
- 从精确 path diff bytes 计算的 hunk SHA-256。

结果 `ValidatedGitSourceArtifact` 没有公开构造器，也不能 Deserialize。Delivery 只消费这份
opaque fact，并再次要求 ArtifactId/digest 出现在同一个已结算成功 Worker outcome 中，且
Job、attempt、Lease、fence、Worker、WorkerSession、CodexThread、StageRun、结束时间和最后
sequence 全部一致。调用方自报 Git 字段、旧候选 Artifact、foreign repository 或另一个
Worker 的产物都不能形成候选。

## 公开边界与验证

Control Plane 的组合入口是 `ControlPlane::start_local` 或
`ControlPlane::start_with_artifacts`。消息入口是 `accept_artifact_open` 和
`accept_artifact_chunk`；源码 adapter 只能安装一次，候选读取入口是
`resolve_delivery_candidate`。通用状态提交入口不拥有这些 bytes 或源码事实。

机器门禁实际执行下面三组 Rust 黑盒测试，而不是只查测试名：

1. `winwincode-storage`：本地/Fake adapter、并发、断点重启、权限、retention、删除、对象与
   catalog corruption、strict Git manifest、replace refs 和继承 Git 环境；
2. `winwincode-control-plane`：generated Artifact 消息、durable Job/Binding/Lease join、
   ACK 错误与候选重建；
3. `winwincode-delivery`：opaque source fact 与精确成功 Worker outcome 才能冻结候选。

本阶段没有保留调用方自报 commit/tree/diff/path 的旧候选路径，也没有声称企业对象存储或
GitHub Publication 已经完成。后者由阶段 3 后续任务继续实现。
