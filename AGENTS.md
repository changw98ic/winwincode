# Agent 工作规则

## 范围与事实来源

本仓库构建 WinWinCode。产品由浏览器 Client、Server、Worker、Device Client、本地命令行和内嵌 Codex 内核组成。

先读 [文档索引](docs/README.md) 和相关领域文档。当前实现以源码、schema、构建脚本和测试为准。架构目标以 [ADR](docs/adr/README.md) 为准。尚未完成的迁移必须明确标记。

## 工作流程

1. 先检查 `git status`。保留已有改动和用户数据。
2. 用 `bd ready`、`bd search`、`bd show` 找到任务。没有对应任务时先用 `bd create` 建立任务。
3. 开始工作时用 `bd update <id> --claim` 认领任务。
4. 完成改动后运行相关检查。将实际结果和未验证项写入任务记录。
5. 只关闭已经满足验收条件的任务。交付时报告文件、检查结果和剩余问题。

Beads 是共享任务的记录入口。不要另建 Markdown 任务账本，也不要用 `MEMORY.md` 保存项目记忆。使用 `bd remember` 保存长期决定。不要运行会打开编辑器的 `bd edit`。

提交、推送、合入和发布遵循当前任务的明确授权。临时工作树默认交付可审查的本地改动。

## 代码检索

代码检索使用 CodeGraph。已知路径的文件可以直接读取。

```sh
codegraph status
codegraph query ProductSession -l 30
codegraph node -f apps/client/src/application.ts --symbols-only
codegraph callers mountWinWinCodeClient
codegraph affected apps/client/src/application.ts
```

索引缺失时先初始化。索引过期时先更新。不要把不完整的调用图当作完整依赖证据。

## 源码与文档

第一方源码不写注释。SPDX 许可证头是唯一豁免项。设计理由、模块职责和运行约束写入领域文档。见 [ADR-0001](docs/adr/0001-source-code-comments.md)。

第三方源码和上游补丁保留上游注释。Shebang 是执行入口，保留。

删除具有编译器、测试或构建作用的注释指令前，先替换为等价检查。清理说明性注释前，先把有效约束写入 [不变量](docs/invariants.md)。

中文文档使用短句和明确主语。同一概念使用同一名称。路径、命令和代码标识符保持原样。引用源码时使用文件路径和符号名。

## 前端

新增页面和组件使用 React 函数组件与 TSX。现有 DOM 页面按 [ADR-0002](docs/adr/0002-react-ui.md) 迁移。

保留视图模型的业务语义。迁移约定使用 `useSyncExternalStore` 接入订阅状态。不要同时改动页面行为、视觉和状态协议。

迁移时保留 class、`data-*`、ARIA 属性和交互行为。视觉验收使用真实渲染截图与设计稿评审。指纹样式测试不作为视觉验收证据。

TypeScript 负责表现层和协议消费。不要在浏览器中复制 Rust 的执行授权和业务状态写入逻辑。

## 运行时与作用域

Server 处理产品状态与命令。Worker 处理受租约约束的执行。内嵌 Codex Core 处理模型、工具和任务执行生命周期。边界见 [架构总览](docs/architecture/overview.md)。

目标作用域是单用户、单工作区、多项目。现有四级作用域仍在协议和存储中使用。变更必须统一更新 schema、生成代码、运行时和测试。见 [ADR-0004](docs/adr/0004-scope-model.md)。

产品数据库使用唯一当前 schema。新库原子初始化。结构不匹配时明确拒绝打开。不得自动回填旧结构，不得擅自删除开发数据。运行中的崩溃恢复仍须保留真实调用事实。

## 生成代码与第三方源码

schema 是协议的事实来源。通过 `pnpm contracts:generate` 更新生成代码，通过 `pnpm contracts:check` 检查一致性。不要手改生成文件。

修改 `third_party/codex` 时，同步维护可重放的上游补丁、来源锁和校验摘要。升级流程见 [上游维护](docs/upstream.md)。保留第三方许可证和 NOTICE。

workspace 内的 crate 和 npm 包是产品内部模块。发布规则见 [ADR-0003](docs/adr/0003-single-product.md)。

## 验证与交付

按 [测试指南](docs/testing.md) 选择检查。纯文档改动检查格式、链接、命令和事实。代码改动运行受影响的定向测试。里程碑收口再运行完整门禁。

真实编程任务的验收使用真实 Provider。确定性 fixture 只能作为明确标记的对照或组件测试。不得把模型自报成功、命令退出或历史测试结果当作本次产品验收通过。

浏览器构建会写入共享 `apps/client/dist`。避免并发构建覆盖。并行验证使用隔离的输出和数据目录。提供给用户继续验收的服务保持运行。

不得提交凭据、运行数据库、缓存、日志和测试产物。安全边界见 [安全模型](docs/security-model.md)。
