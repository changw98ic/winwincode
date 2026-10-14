# WinWinCode 安全政策

本文说明 WinWinCode 的漏洞报告方式。产品安全边界见[安全模型](docs/security-model.md)。

## 版本状态

| 版本 | 状态 |
| --- | --- |
| 0.2.0 | 当前版本 |

历史版本的支持范围尚未公布。报告问题时，请写明受影响的版本、提交和运行平台。

## 私密报告漏洞

仓库已启用 GitHub 私密漏洞报告。请使用以下入口：

[私密漏洞报告](https://github.com/changw98ic/winwincode/security/advisories/new)

报告应包含以下信息：

- 受影响的组件和版本。
- 可重复的最小步骤或概念验证。
- 实际影响和必要前提。
- 相关配置、日志和错误信息。请先删除凭据、令牌、个人数据和仓库私有内容。
- 已知缓解方法。

如果该入口因仓库设置或账户权限而不可用，请不要在公开 Issue、Pull Request 或讨论区披露敏感细节。请保留材料，等待可用的私密渠道。

## 报告范围

可以报告影响以下边界的问题：

- Browser Client、Server、Device Client、Worker、`wwc` 和内嵌 Codex 内核。
- 登录、会话、设备接入、执行租约、消息重放和权限校验。
- Provider 凭据、MCP、工具调用、sandbox 和网络边界。
- 发布包、helper 签名、构建来源和上游补丁。

如果上游 Codex 缺陷能够影响 WinWinCode，请在本仓库私密报告。维护者可以再协调上游披露。

测试时请使用自己控制的账户、设备、Provider 和仓库。不要访问第三方数据。不要提交真实凭据。不要让概念验证执行不可恢复的数据修改。

开发和提交规则见[贡献指南](CONTRIBUTING.md)。文档入口见[文档索引](docs/README.md)。
