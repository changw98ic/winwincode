# ADR-0001：第一方源码的说明写入文档

- 状态：已接受
- 日期：2026-10-14
- 实施状态：文档已建立，源码清理与检查待实施

## 背景

模块职责、设计理由和运行约束需要统一的阅读入口。源码中的说明容易与架构文档分散维护。

## 决定

第一方源码不写注释。SPDX 许可证头是唯一豁免项。模块说明写入架构文档。跨模块约束写入 [不变量](../invariants.md)。长期设计选择写入 ADR。

第三方源码、vendored 依赖和上游补丁豁免。维护它们时保留上游注释和许可信息。Shebang 是执行入口，保留。

第一方 SPDX 头继续保留，生成器和构建横幅中的 SPDX 头也保留。仓库中的 `LICENSE`、`NOTICE`、`THIRD_PARTY_NOTICES.md` 和第三方许可文件继续保留。

## 实施边界

此决定覆盖第一方 Rust、TypeScript、JavaScript、CSS、Shell 和配置源码中的注释。Markdown 文档是设计说明的载体。

删除现有注释前，先迁移仍有效的约束。编译器、测试和构建指令必须先改成等价实现，再移除相应注释。

例如 `tests/fixtures/browser-control-type-gate.ts` 使用 `@ts-expect-error` 验证错误调用。清理时必须保留对错误诊断的检查，不能直接删掉负例。

生成器和构建横幅也需要统一处理。更新生成器后重新生成文件，再用 `pnpm contracts:check` 验证一致性。

## 影响与验收

现有源码注释尚未全量清理。当前 `pnpm lint:source` 尚未提供覆盖所有第一方语言的零注释检查。新增代码按本决定编写。

实施完成需要迁移有效说明、保留行为检查，并将可靠的注释检测接入源码门禁。检测须区分字符串、正则、shebang、SPDX 头和其他实际注释。

## 依据

- `scripts/lint-workspace.mjs`
- `scripts/generate-contracts.mjs`
- `scripts/generate-network-policy.mjs`
- `apps/client/scripts/build.mjs`
- `tests/fixtures/browser-control-type-gate.ts`
- [上游维护](../upstream.md)
