# ADR-0031：三个产品使用三个代码仓库

- 状态：已接受
- 日期：2026-09-05
- 对应任务：`winwincode-edition.1`（REPO-SPLIT-000.1）
- 机器可读仓库清单：[0031-product-editions.json](0031-product-editions.json)
- 核心运行边界：[ADR-0028](0028-control-plane-worker-migration.md)

## 背景

WinWinCode 需要分别服务个人开发者、SaaS 用户和私有化客户。三个产品有不同的身份体系、
存储、Worker 来源、部署方式、升级节奏和安全责任。源码和任务必须按产品归入独立仓库，确保
每个仓库只包含该产品及其直接需要的内容。

## 决定

建立三个 Git 仓库：

| 仓库 | 产品 | 主要职责 |
| --- | --- | --- |
| `winwincode` | WinWinCode Community | 个人开源产品、开放执行核心、公共协议与核心发布物 |
| `winwincode-cloud` | WinWinCode Cloud | SaaS 多租户、计费、云 Worker、托管运维与 Cloud 应用 |
| `winwincode-enterprise` | WinWinCode Enterprise | 私有化身份、管理、部署、审计、备份与 Enterprise 应用 |

三个仓库分别拥有自己的 Git 历史、Beads 数据库、CI、版本号、发布物和安全发布流程。

### 1. Community 仓库

当前 `winwincode` 仓库整理为 Community 产品与开放核心仓库。它包含：

- DSH Chat 和 StrongFlow 的个人产品入口；
- Codex Core 集成、ExecutionPort、Worker 和 Kernel Helper；
- Repository、Candidate 和本地 Git 交付基础；
- 中立的领域对象、公共 schema 和生成协议；
- 供另外两个仓库消费的版本化 Rust crates、npm packages、SBOM 和校验摘要；
- 本机组合、安装、升级和恢复。

Community 面向单用户、本机项目和本地 Worker。本机产品数据使用 SQLite；Codex Core 的
SQLite 继续由内核独占。项目自有代码使用 Apache-2.0。

Community 的身份模型只有一个本地 Owner：首次启动用 bootstrap proof 创建 Owner，之后
通过 Owner 用户名和密码登录；允许同一 Owner 同时拥有多个浏览器会话和连接多台 Device
Client。Owner 只能修改自己的凭据。组织用户、Member、团队、RBAC、账号生命周期管理和
向其他用户授予 Client/Repository 权限属于 Enterprise，不进入 Community 的 Server、Web、
schema 或生成合同。

Community 核心发布物只包含执行和公共协议能力。组织管理、租户、计费、Hosted 运维、
私有化部署和产品管理页面归入各自产品仓库。

### 2. Cloud 仓库

`winwincode-cloud` 包含：

- Cloud Server 和 Cloud Web 产品入口；
- 多租户组织、成员、项目和隔离规则；
- PostgreSQL 后端与托管对象存储；
- SaaS 身份、套餐、额度和计费；
- 官方云 Worker 和托管调度；
- Hosted 监控、备份、恢复、容量和发布门。

Cloud 通过精确版本和摘要消费 Community 发布的核心包。租户边界由 Server、数据库、
对象存储、审计和 Worker 调度共同强制执行。

### 3. Enterprise 仓库

`winwincode-enterprise` 包含：

- Enterprise Server 和 Enterprise Web 产品入口；
- 组织内用户、团队、Repository 权限和客户身份；
- PostgreSQL 后端与客户选择的对象存储；
- 审计、策略、密钥、备份、恢复和升级；
- 私有化安装、受限网络和离线运维；
- 客户自管 Device Client 与 Worker。

Enterprise 通过精确版本和摘要消费 Community 发布的核心包。产品在客户网络中独立运行，
部署、身份、密钥和数据由客户环境管理。

## 跨仓依赖

Community 在正式 release 中发布：

- Rust 核心 crates；
- TypeScript 公共 packages；
- JSON Schema、OpenAPI 和协议样本；
- 版本 manifest、SBOM、许可证清单和 SHA-256 摘要。

Cloud 和 Enterprise 各自保存 `core lock manifest`，记录核心版本、提交、协议版本、包版本
和摘要。构建只能使用 manifest 指向的正式发布物，不能引用另一个仓库的本地工作目录。

每次核心升级在消费仓库中作为一次完整变更进行：更新核心依赖、重新生成协议、运行产品测试、
更新发布清单。一个运行产物只支持它锁定的当前协议，不维护多个旧产品协议路径。

```text
winwincode core release
  ├── core crates + npm packages + schema + checksums
  ├──────────────> winwincode-cloud/core.lock.json
  └──────────────> winwincode-enterprise/core.lock.json
```

## 数据库边界

| 数据 | Community | Cloud | Enterprise |
| --- | --- | --- | --- |
| 产品业务数据 | 本机 SQLite | PostgreSQL | PostgreSQL |
| Codex Core thread/turn/state | SQLite | SQLite | SQLite |
| Device Client 身份、游标、路径和恢复数据 | SQLite | SQLite | SQLite |
| 大对象 | 本机文件 | 托管对象存储 | 客户对象存储或本地实现 |

Cloud 和 Enterprise 的 Server/Control Plane 后端迁移到 PostgreSQL。Codex Core 和
Device Client SQLite 保持各自的本地所有权。

## 源码迁移

迁移按以下顺序执行：

1. 生成当前源码、测试、文档和 Beads 的归属清单；
2. 创建 Cloud 和 Enterprise 仓库，建立 AGENTS、Beads、许可证、CI 和安全默认值；
3. 在 Community 仓库冻结第一版核心发布物；
4. 使用保留提交来源的 Git 拆分方式迁移 Cloud 与 Enterprise 文件；
5. 在目标仓库接入核心 lock manifest 并通过干净构建；
6. 从 Community 删除已迁移的产品源码，更新构建和测试清单；
7. 分别运行三个仓库的产品测试和跨仓升级测试。

迁移中的文件只能有一个最终源码仓库。临时复制文件在目标仓通过验收后立即从来源仓删除。

## 任务归属

三个仓库分别使用自己的 Beads：

- 核心执行或 Community 产品任务进入 `winwincode-community`；
- SaaS、租户、计费、Cloud Worker 和 Hosted 运维任务进入 `winwincode-cloud`；
- 私有化身份、管理、部署、审计和客户运维任务进入 `winwincode-enterprise`。

跨仓任务用正式核心 release 或协议版本作为前置条件。一个实现任务只存在于拥有该源码的仓库。

## 验收结果

- 三个仓库都能从干净检出独立构建和测试；
- Community 发布核心后，Cloud 与 Enterprise 能按精确锁定版本构建；
- 每个产品仓库的源码扫描只允许本产品目录和公共核心依赖；
- 三个发布物拥有独立名称、版本、SBOM、签名和升级记录；
- Cloud 和 Enterprise 的后端使用 PostgreSQL；
- Codex Core 与 Device Client 的 SQLite 边界保持不变。
