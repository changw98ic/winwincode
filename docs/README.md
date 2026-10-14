# 文档索引

## 使用与开发入口

| 文档 | 内容 |
| --- | --- |
| [项目说明](../README.md) | 产品组成、环境与常用入口 |
| [参与开发](../CONTRIBUTING.md) | 修改流程、检查与 PR |
| [Agent 工作规则](../AGENTS.md) | 任务、检索、源码和验证规则 |
| [安全政策](../SECURITY.md) | 漏洞报告与支持范围 |
| [部署](operations/deploy.md) | 配置、启动、数据与备份恢复 |
| [发布](operations/release.md) | 版本、平台、制品与验收 |

## 实现说明

| 文档 | 内容 |
| --- | --- |
| [架构总览](architecture/overview.md) | 进程、通信、执行流程、存储与依赖 |
| [Rust 模块](architecture/crates.md) | crate 职责、入口与依赖 |
| [前端架构](architecture/frontend.md) | 挂载、路由、状态、样式与 React 迁移 |
| [契约](contracts.md) | schema、生成代码、命令和消息 |
| [测试](testing.md) | 定向检查、完整门禁与环境要求 |
| [安全模型](security-model.md) | 身份、凭据、租约、沙箱和发布信任 |
| [不变量](invariants.md) | 执行与持久化必须保持的约束 |
| [术语](glossary.md) | 产品和运行时名称 |
| [上游维护](upstream.md) | 内嵌 Codex、补丁和来源锁 |

## 架构决定

| ADR | 决定 |
| --- | --- |
| [0001](adr/0001-source-code-comments.md) | 第一方源码的说明写入文档 |
| [0002](adr/0002-react-ui.md) | 页面和组件统一使用 React |
| [0003](adr/0003-single-product.md) | 仓库构建和发布单一产品 |
| [0004](adr/0004-scope-model.md) | 单用户、单工作区、多项目 |

新增决定使用 [ADR 模板](adr/template.md)。
