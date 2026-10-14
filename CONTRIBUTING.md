# 参与开发

本文分两部分：第一部分写给人类贡献者，第二部分是 AI Agent 和维护者需要遵守的详细规则。

## 给贡献者：快速上手

### 准备环境

工具版本和构建命令见 [README：从源码构建和测试](README.md#从源码构建和测试)。完整依赖（libclang、V8 构建等）见[部署指南](docs/operations/deploy.md#从源码构建)。

### 提交改动

1. 从 `main` 新建分支。
2. 修改后，按[定向验证](docs/testing.md#定向验证)运行与改动相关的检查。只改文档时，至少运行 `corepack pnpm verify:docs`。
3. 提交标题沿用现有格式，例如 `fix(scope): ...`、`docs: ...`。
4. PR 描述写清：问题是什么、改后行为、跑了哪些检查、哪些没验证。
5. 安全漏洞不要开公开 issue，按 [SECURITY.md](SECURITY.md#私密报告漏洞) 私密报告。

### 文档写法

中文用短句和明确主语。同一概念用同一名称（见[术语表](docs/glossary.md)）。路径、命令和代码标识符保持原样。写清当前实现和尚未实现的目标的区别。

## 给 Agent 和维护者：详细规则

Agent 的全局执行边界见 [AGENTS.md](AGENTS.md)。按[文档索引](docs/README.md)查找当前任务的领域规则。

### 协作与任务

本仓库用 [Beads](https://github.com/steveyegge/beads)（命令行工具 `bd`）跟踪任务。Beads 是一个基于 Git 的轻量 issue 跟踪器，任务数据保存在仓库的 `.beads/` 目录中。

安装（任选其一，详见 Beads 官方安装文档）：

```sh
brew install beads
npm install -g @beads/bd
```

人类贡献者提交一次性的小改动时可以不用 `bd`，在 PR 中说明即可。Agent 和持续维护者按以下流程：

1. 用 `bd ready`、`bd search`、`bd show` 查找已有任务。
2. 没有对应任务时，用 `bd create` 新建。
3. 用 `bd update <id> --claim` 认领。
4. 完成后，把实际检查结果和未验证项写入任务记录。
5. 只关闭已满足验收条件的任务。

任务记录目标、范围、验收和结果；长期规则写进对应的领域文档，不写进任务。不要另建 Markdown 任务账本或 `MEMORY.md`，不要用 `bd remember` 复制文档正文，不要运行会打开编辑器的 `bd edit`。

修改规则时更新它所属的文档并调整引用。协议改动见[契约](docs/contracts.md#生成文件规则)。第三方源码改动见[上游维护](docs/upstream.md#修改和升级规则)。

### 源码与注释

**目标规则：** 第一方源码（Rust、TypeScript、JavaScript、CSS、Shell、配置，以及生成器和构建横幅产生的内容）不写注释。例外：SPDX 许可证头、shebang、第三方源码、vendored 依赖和上游补丁（保留上游注释和许可信息，见[上游维护](docs/upstream.md#许可证)）。模块职责和设计理由写进领域文档，跨模块约束写进[不变量](docs/invariants.md)。

**现状：** 这条规则尚未完全落实。现有第一方源码里仍有注释，`pnpm lint:source` 也还不能检查所有语言的注释。因此：

- 新增代码不要加注释。
- 不强制你清理所改文件里已有的注释。要清理时，先把仍有效的约束迁到文档。
- 编译器、测试或构建指令（例如 `tests/fixtures/browser-control-type-gate.ts` 中的 `@ts-expect-error`）不能直接删除，必须先替换为等价检查。
- 全仓清理和检测器建设需要单独立项。检测器必须区分字符串、正则、shebang、SPDX 头和真实注释。

更新生成器后，重新生成受影响文件，并按[生成文件规则](docs/contracts.md#生成文件规则)检查一致性。保留 SPDX 头、`LICENSE`、`NOTICE`、`THIRD_PARTY_NOTICES.md` 和第三方许可文件。

相关实现：`scripts/lint-workspace.mjs`、`scripts/generate-contracts.mjs`、`scripts/generate-network-policy.mjs`、`apps/client/scripts/build.mjs`。

### 验证与提交

按[定向验证](docs/testing.md#定向验证)选择检查。真实 Provider 证据和视觉检查方法见测试指南。提交前检查 diff。
