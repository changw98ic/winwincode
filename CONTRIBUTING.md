# 参与开发

开发环境和构建命令见 [README](README.md)。实现结构见 [架构总览](docs/architecture/overview.md)。Agent 工作规则见 [AGENTS.md](AGENTS.md)。

## 修改流程

用 Beads 记录任务。修改前检查工作树，保留已有改动。提交一个明确范围的改动，并同步更新对应文档。

协议的事实来源是 `schema/winwincode/v1`。修改 schema 后运行：

```sh
pnpm contracts:generate
pnpm contracts:check
pnpm typecheck
```

生成文件随 schema 一起提交。内嵌 Codex 的修改还需要更新补丁和来源锁，步骤见 [上游维护](docs/upstream.md)。

## 验证

| 改动 | 检查 |
| --- | --- |
| 文档 | `node scripts/check-format.mjs`，核对链接、命令和事实 |
| TypeScript | `pnpm typecheck` 和对应测试；交互变化增加浏览器检查 |
| Rust | 对应 crate 的测试、`cargo fmt --all -- --check` 和 Clippy |
| 协议 | `pnpm contracts:check` 和相关契约测试 |
| 内核 | 补丁重放测试、内核测试和原生执行检查 |

具体命令和环境前提见 [测试指南](docs/testing.md)。开发时运行受影响的检查，完整组合门禁使用 `pnpm verify`。

界面变化用真实截图对照设计稿。发布 API 检查使用确定性模型 fixture；真实 Provider 编程任务的质量验收需单独留证。

## 提交与 PR

提交标题沿用 `fix(scope): ...` 等现有格式。PR 描述说明问题、修改后的行为和实际验证结果，并列出未验证的范围。

提交前检查 diff。凭据、运行数据库、缓存和测试产物留在本地。漏洞报告使用 [私密渠道](SECURITY.md)。
