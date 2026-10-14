# ADR-0004：单用户、单工作区、多项目

- 状态：已接受
- 日期：2026-10-14
- 实施状态：协议、领域模型与持久化简化待实施

## 决定

WinWinCode 面向单个用户。产品只有一个工作区。工作区可以包含多个项目。

保留稳定的 Owner 身份。仓库绑定、任务、会话、凭据和审计继续记录各自的有效归属。项目之间保持明确边界。

## 当前实现

现有协议、领域模型和存储仍使用 organization、workspace、project、repository 四级作用域。`OrganizationId`、`WorkspaceId`、`ProjectId`、`RepositoryId` 仍是实际使用的标识符。

四级作用域的存在不代表产品提供多用户管理。当前单 Owner 身份与作用域协议是不同概念。

## 实施边界

简化作用域需要同时修改 schema、生成代码、Server、Worker、Device Client、持久化和测试。不要仅改名称或在单一层丢弃归属字段。

保留项目与仓库绑定的必要关系。具体项目和仓库基数以统一后的领域模型与契约为准，不能从本决定推导成单仓库限制。

数据库采用唯一当前 schema。新库原子初始化，结构不匹配时拒绝打开。不为未发布旧结构添加自动升级、回填或兼容路径，也不擅自清空已有数据。

## 影响与验收

产品配置围绕单用户和单工作区收敛。项目身份继续用于任务、会话、仓库与审计。

实施完成需要跨层契约一致，并验证多个项目的归属、执行授权和持久化隔离。现有四级作用域在统一迁移前仍须完整校验。

## 依据

- `schema/winwincode/v1/domain.schema.json`
- `schema/winwincode/v1/execution-port.schema.json`
- `crates/winwincode-domain/src/generated.rs`
- `crates/winwincode-server/src/main.rs`
- [契约](../contracts.md)
- [架构总览](../architecture/overview.md)
