# ADR-0031：单一后端 PostgreSQL 与 SQLite 迁移边界冻结

- 状态：已接受
- 日期：2026-09-06
- 对应任务：`winwincode-9c4.21.1`（PG-000）
- 机器可读表清单：[`0031-single-backend-postgres.inventory.json`](0031-single-backend-postgres.inventory.json)
- 上层运行边界：[ADR-0028](0028-control-plane-worker-migration.md)
- Client/设备占用边界：[ADR-0030](0030-multi-user-client-access-and-occupancy.md)

## 背景

当前 Server、Control Plane、storage、audit、session、observability、integration 与
winwincode-codex 各自直接持有 SQLite（rusqlite）数据库：Control Plane 状态权威在
`control-plane.sqlite3`，Server 另有 `auth-sessions.sqlite3`、`server-event-hub.sqlite3`、
`client-occupancy-mirror.sqlite3`，企业域有 `audit.sqlite3`、`integration.sqlite3`、
`slack-rate-limits.sqlite3`，Worker 侧有 `worker-codex.sqlite3`，设备侧有
`device-client.sqlite3`，另有 session 一次性迁移簿记与内嵌 Codex Core 的 home state。
多用户（ADR-0030）之后，同一后端要面向多个用户与多台设备，本机单文件 SQLite 不再
承载后端权威状态的部署形态。

既有 `crates/winwincode-postgres` v1 只有 7 张表
（`winwincode_schema_migrations`、`winwincode_product_state`、
`winwincode_command_receipts`、`winwincode_outbox`、`winwincode_aggregate_journals`、
`winwincode_aggregate_journal_records`、`winwincode_audit_outbox`），且每张业务表都
`FORCE ROW LEVEL SECURITY` 并依赖 `current_setting('winwincode.scope_key')` 的
`scope_key` 会话变量策略。它只是把 storage 核心事件存储镜像了一遍，没有 auth、audit
链、事件枢纽、observability、integration、artifact catalog、client/device、执行调度等任何
表，并保留了已废弃的多租户 RLS 设计。**它不是后端目标结构，不能当作完整后端 schema
使用。**

本决定在进入 PG-010/020/030 之前，从代码里逐表清点 SQLite 使用（所有 `CREATE TABLE`
与直接连接点），冻结"哪些迁、哪些留"的边界。

## 决定

### 1. 单一后端数据库：PostgreSQL

后端权威状态只有一个数据库：PostgreSQL。Server 与 Control Plane 的全部后端域
（状态、回执、outbox、身份、授权、占用、调度、租约、企业治理、审计、事件枢纽、
可观测性、集成、artifact catalog）都落在这一 PostgreSQL 中，按 database 划分
pgSchema（`control_plane`、`server_auth`、`event_hub`、`audit`、`integration`、
`observability`、`artifact_catalog`、`occupancy_mirror`）。

### 2. 禁止双数据库运行路径

运行时不允许出现"同一份权威事实同时可写 SQLite 与 PostgreSQL"的双写或双读路径。
迁移按域一次切换：切换前 SQLite 是唯一权威，切换后 PostgreSQL 是唯一权威；SQLite
只作为迁移期的一次性数据源，cutover 后后端代码不得再打开后端域的 SQLite 文件。
`winwincode-cli` 的备份/诊断在过渡期继续按现有方式访问遗留 SQLite（诊断只读，
备份快照允许 WAL checkpoint）。

### 3. 迁移与保留边界

逐表处置冻结在
[`0031-single-backend-postgres.inventory.json`](0031-single-backend-postgres.inventory.json)：
147 张表，`migrate` 116 张、`retain` 31 张，并满足两条机器可校验不变量——
每个 `crates/` 下的 `CREATE TABLE` 名都出现在清单中（entry、legacy target 或显式
nonInventoryName），且外键引用表的迁移顺序不晚于引用方。守卫脚本是
`scripts/db-migration-inventory-contract.mjs`。

保留（`retain`）的三类，均不迁 PostgreSQL：

- **Codex 内核 / Worker 本地运行时**（`worker-codex.sqlite3`，11 张）：
  `codex_run`、model ledger/frames、`runtime_replay`、`execution_outbox`、
  approval/input operation 等生命周期与 Worker/CodexThread 绑定，是执行内核的
  本机状态；内嵌 Codex Core 自己的 home state（`state.sqlite3`，upstream 所有）
  一并保留，其 schema 不归本仓库清单管辖；
- **设备端**（`device-client.sqlite3`，17 张）：设备身份、凭据、本地仓库绝对路径
  mapping、占用镜像与本地 fencing 强制、worker 进程注册、candidate 本地 ref 与
  设备 outbox。ADR-0030 已冻结"绝对路径与凭据只留在 Device Client"，这些表迁到
  后端即违约；
- **一次性迁移簿记**（3 张 `session_identity_migration_*`）：legacy 交付快照转换的
  source/snapshot/consumed 标记，写入一次、cutover 后即可弃置，不是后端状态。

### 4. 退役 winwincode-postgres v1 的租户 RLS 设计

旧的多租户 PostgreSQL/RLS 设计（`scope_key` 会话变量 + `FORCE ROW LEVEL
SECURITY` + per-table POLICY）就此退役。后端授权边界是 Control Plane 应用层的
scope 检查（ADR-0028/0030 的权威表），不是数据库行级安全策略；`organization_id`、
`scope_key` 作为普通数据列保留，用于审计与归属，不承载访问控制。PG-020 重设计
schema 时不得重新引入 RLS 作为授权机制。

### 5. 目标表命名与迁移顺序

默认目标表名为 `<pgSchema>.<源表名>`（`targetNaming: derived-schema-per-database`），
PG-020 可逐表覆盖但必须保持清单 entry id（`<databaseId>:<sourceTable>`）稳定。
`migrationOrder` 按外键拓扑分层：核心事件存储（`product_state`、`command_receipts`、
`outbox`、`aggregate_journals` 及其 heads/records）最先，其后依次是 identity、
repository、client access/occupancy、执行调度、企业治理、provider exchange、
artifact catalog、audit、server auth/event hub、observability、integration。
每张表都带 `volumeCheck`（迁移前 SQLite 侧 `SELECT COUNT(*)` 与量级预期），
PG-030 的对账以此为准。

### 6. 不改变现有运行行为

本决定只冻结合同：清单 JSON、本 ADR 与守卫脚本。任何 SQLite schema、连接或迁移
代码的行为都不在本任务中改动；winwincode-postgres v1 的表继续原样存在，直到
PG-020 用新 schema 取代。

## 后果与取舍

收益：

- 后端只有一个权威数据库，多用户/多设备的部署、备份、升级与对账有了单一落点；
  PG-010/020/030 拿到的是逐表、带主键/外键/顺序/量级检查的冻结清单，而不是"从
  SQLite 重新考古"；
- 内核、设备端、一次性簿记被显式留在外面，避免把本地 fencing、绝对路径与凭据
  意外搬进后端；
- RLS 退役把授权收回到 Control Plane 一处，删除了 scope 会话变量这条隐蔽的
  授权通道。

代价与风险：

- PostgreSQL 成为新的运维依赖（备份、版本、连接管理），本机单文件部署形态需要
  PG-010 给出内嵌/捆绑方案；
- "单后端 + 设备保留 SQLite"意味着 cutover 前后存在两套持久化代码路径并存的
  迁移窗口，必须靠本清单与守卫脚本防止双写路径固化成第二运行形态；
- 清单按当前代码冻结，后续新增表必须同步更新 inventory 并通过
  `scripts/db-migration-inventory-contract.mjs`，否则清单会腐烂。
