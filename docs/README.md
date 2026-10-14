# 文档索引

## 按任务阅读

| 任务 | 相关正文 | 实现与检查入口 |
| --- | --- | --- |
| 只读检查前端变更 | [React 约定](architecture/frontend.md#react-约定)、[视图模型](architecture/frontend.md#视图模型与-react)、[DOM 契约](architecture/frontend.md#dom-契约和可访问性) | 按改动定位页面；Chat 示例：[mountChatPage](../apps/client/src/chat-page.ts)、[createChatViewModel](../apps/client/src/chat-view-model.ts) |
| 实施页面修改 | [协作](../CONTRIBUTING.md#协作与任务)、[源码规则](../CONTRIBUTING.md#源码与注释)、[React 约定](architecture/frontend.md#react-约定)、[DOM 契约](architecture/frontend.md#dom-契约和可访问性)、[定向验证](testing.md#定向验证) | Chat 示例：[chat-page.test.mjs](../tests/chat-page.test.mjs)；无障碍：[ui604-shell-a11y-browser.test.mjs](../tests/ui604-shell-a11y-browser.test.mjs) |
| 修改 schema 或消息 | [生成流程](contracts.md#生成文件规则)、[协议检查](contracts.md#兼容性检查)、按消息选择 [命令身份](invariants.md#命令身份与修订)、[租约](invariants.md#租约与-fencing) 或 [作用域](invariants.md#作用域) | [domain.schema.json](../schema/winwincode/v1/domain.schema.json)、[generate-contracts.mjs](../scripts/generate-contracts.mjs)；HTTP 示例：[control-plane-http-contract.test.mjs](../tests/control-plane-http-contract.test.mjs) |
| 排查取消、恢复或旧 Worker 消息 | [取消](invariants.md#取消与恢复)、[恢复](invariants.md#崩溃恢复与新-attempt)、[旧消息](invariants.md#旧-worker-的迟到消息) | [model_bridge.rs](../crates/winwincode-codex/src/model_bridge.rs)、[recovery_router.rs](../crates/winwincode-control-plane/src/recovery_router.rs)；旧消息测试：[chat-execution-obsolete-owner-production.test.mjs](../tests/chat-execution-obsolete-owner-production.test.mjs) |
| 部署与备份 | [准备 Server](operations/deploy.md#准备-server)、[备份与恢复](operations/deploy.md#备份校验和恢复)、[网络安全](security-model.md#网络边界) | [run_composed_server](../crates/winwincode-server/src/main.rs)、[discover_databases](../crates/winwincode-cli/src/backup.rs)；[backup.rs 内联测试](../crates/winwincode-cli/src/backup.rs)：`snapshot_and_restore_round_trip_server_ids_cursors_and_receipts` |
| 修改第三方内核补丁 | [升级规则](upstream.md#修改和升级规则)、[补丁重放](upstream.md#补丁重放) | [sources.lock.json](../upstream/sources.lock.json)、[codex.UPSTREAM.json](../third_party/codex.UPSTREAM.json)；[code-mode-patch-replay.test.mjs](../tests/code-mode-patch-replay.test.mjs) |

只读工作遵循 [Agent 执行边界](../AGENTS.md)。实施前阅读贡献指南，再按任务查阅相关章节。

## 主题

| 文档 | 内容 |
| --- | --- |
| [项目说明](../README.md) | 产品与使用入口 |
| [安全政策](../SECURITY.md) | 私密漏洞报告与支持范围 |
| [架构总览](architecture/overview.md) | 产品范围、组件和数据流 |
| [Rust 模块](architecture/crates.md) | 职责、入口和依赖 |
| [前端架构](architecture/frontend.md) | 路由、状态、React 与 DOM 契约 |
| [契约](contracts.md) | schema、生成代码和消息格式 |
| [不变量](invariants.md) | 跨模块执行和持久化约束 |
| [测试](testing.md) | 定向检查、门禁和验收方法 |
| [安全模型](security-model.md) | 身份、凭据、sandbox 和信任边界 |
| [部署](operations/deploy.md) | 环境、构建、配置、数据与恢复 |
| [发布](operations/release.md) | 发布政策、平台和制品流程 |
| [上游维护](upstream.md) | 第三方源码、来源锁和补丁 |
| [术语](glossary.md) | 名称与简短定义 |

## 历史

[ADR 历史入口](adr/README.md) 用于追溯原决定。现行要求由上表中的领域正文维护。
