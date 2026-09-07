# 0032：Community 持久化端口发布边界

Community 的公开持久化端口只表达数据库中立的领域行为：事务、版本/读切面、幂等重放、稳定领域错误和 opaque 存储权威。公开 trait 的签名不携带本地数据库、托管服务或 Enterprise 产品类型。

本地 SQLite 仍由 `winwincode-storage::SqliteStorage` 提供，并只在明确的本地组合入口使用。通用端口通过 `ProductStateStorage` 接收适配器，不根据目录路径重新打开数据库。

## Enterprise 专属持久化类型的处置

七个 Enterprise 专属持久化 trait 不是 Community 边界的一部分，也不得重新出现或重新导出：

`EnterprisePolicyPersistence`、`EnterprisePolicyProductStatePersistence`、`EnterprisePolicyEvaluationPersistence`、`EnterpriseQuotaPersistence`、`EnterpriseUsagePersistence`、`EnterpriseUsageProductStatePersistence`、`EnterpriseUsageReconciliationPersistence`。

它们全部表达 Enterprise 产品语义（policy、quota、usage、reconciliation），没有真正共享的中立领域能力，因此不需要重命名迁移，也不保留旧别名；产品数据库能力由 Cloud/Enterprise 仓库自行组合。机器门按名字拒绝它们出现在 Community 三个 owner crate 的源码与公开导出中。

## 机器门禁的失败关闭性质

机器清单位于 [`0032-community-persistence-ports.inventory.json`](0032-community-persistence-ports.inventory.json)。门禁从源码自动发现 `Persistence` trait 和带 `winwincode-community-persistence-port` 标记的 trait，并把清单与发现结果逐项对齐，因此清单漏掉或虚增端口都会失败。

门禁同时从三个方向拒绝产品类型：

1. **端口签名**：每个被发现的端口（不只清单条目）的 trait 体不得出现 `forbiddenPublicTypePatterns` 中的词，包括 PostgreSQL、Hosted、Enterprise 三类产品词；从模式列表里删掉任一产品词也会失败。
2. **公开导出**：owner crate 根导出的标识符不得把产品词与 `Persistence` 组合；每个含 `Persistence` 的导出必须是已冻结端口；端口必须能在其 crate 的公开导出面上到达；glob 再导出被直接拒绝，因为它会隐藏导出名。
3. **源码与导出中的已移除类型**：七个已移除的 Enterprise 类型名不得出现在 owner crate 源码或导出的任何位置。

每个冻结端口还必须带有仍然存在的 Rust 行为证据；证据符号是该端口的本地适配器或消费它的服务。

## 运行门禁

```bash
node --test tests/community-persistence-ports.test.mjs
```

该门已接入 canonical runner `scripts/run-ts-tests.mjs`，因此仓库测试通道会持续执行它。
