# 0032：Community 持久化端口发布边界

Community 的公开持久化端口只表达数据库中立的领域行为：事务、版本/读切面、幂等重放、稳定领域错误和 opaque 存储权威。公开 trait 的签名不携带本地数据库、托管服务或 Enterprise 产品类型。

本地 SQLite 仍由 `winwincode-storage::SqliteStorage` 提供，并只在明确的本地组合入口使用。通用端口通过 `ProductStateStorage` 接收适配器，不根据目录路径重新打开数据库。

机器清单位于 [`0032-community-persistence-ports.inventory.json`](0032-community-persistence-ports.inventory.json)，由源码自动发现 `Persistence` trait 和带 `winwincode-community-persistence-port` 标记的 trait。Enterprise 专属持久化 trait 不属于 Community 清单，也不允许重新导出。

运行门禁：

```bash
node --test tests/community-persistence-ports.test.mjs
```

该门检查清单与三个 owner crate 的公开端口一致、公开错误保持稳定、端口签名不暴露数据库/托管/Enterprise 类型，并确认清单中列出的 SQLite 原子提交证据仍存在。
