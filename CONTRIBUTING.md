# 参与开发

环境与构建见 [部署指南](docs/operations/deploy.md#从源码构建)。Agent 的全局执行边界见 [AGENTS.md](AGENTS.md)。按 [文档索引](docs/README.md) 查找当前任务的领域规则。

## 协作与任务

实施使用 Beads 记录任务差量：目标、范围、相关正文章节、任务特有验收、进度和实际结果。

1. 用 `bd ready`、`bd search`、`bd show` 定位已有任务。
2. 没有对应任务时，用 `bd create` 建立任务。
3. 用 `bd update <id> --claim` 认领任务。
4. 完成改动后，将实际检查结果和未验证项写入任务记录。
5. 只关闭已满足验收条件的任务。

任务引用领域规则，并补充本次工作差量。长期规范及必要设计理由直接维护在对应正文。历史任务和执行证据保留，不改写为新的验收结论。

不要另建 Markdown 任务账本或 `MEMORY.md`。不要用 `bd remember` 复制领域正文。已有记忆需要对齐时，先保留独特约束，再补充现行正文入口。不要运行会打开编辑器的 `bd edit`。

修改规则时更新它所属的正文，并调整受影响的引用。协议改动见 [契约](docs/contracts.md#生成文件规则)。第三方源码改动见 [上游维护](docs/upstream.md#修改和升级规则)。

## 源码与注释

第一方源码不写注释。SPDX 许可证头是唯一豁免项。此规则覆盖 Rust、TypeScript、JavaScript、CSS、Shell 和配置源码，也覆盖生成器与构建横幅产生的注释。

第三方源码、vendored 依赖和上游补丁豁免。维护它们时保留上游注释和许可信息，操作要求见 [上游维护](docs/upstream.md#许可证)。Shebang 是执行入口，保留。

模块职责与设计理由写入领域文档。跨模块运行约束写入 [不变量](docs/invariants.md)。统一阅读入口可以避免有效说明分散维护。

删除说明性注释前，先迁移仍有效的约束。删除编译器、测试或构建指令前，先替换为等价实现和检查。例如 `tests/fixtures/browser-control-type-gate.ts` 使用 `@ts-expect-error` 检查错误调用，清理时必须保留错误诊断检查。

更新生成器后，重新生成受影响文件，并按 [生成文件规则](docs/contracts.md#生成文件规则) 检查一致性。保留 SPDX 头、`LICENSE`、`NOTICE`、`THIRD_PARTY_NOTICES.md` 和第三方许可文件。

当前源码尚有其他注释。`pnpm lint:source` 尚未提供覆盖全部第一方语言的零注释检查。已有注释的清理和检测器建设需独立实施；检测器须区分字符串、正则、shebang、SPDX 头与实际注释。

依据：`scripts/lint-workspace.mjs`、`scripts/generate-contracts.mjs`、`scripts/generate-network-policy.mjs`、`apps/client/scripts/build.mjs`、`tests/fixtures/browser-control-type-gate.ts`。

## 文档

中文使用短句和明确主语。同一概念使用同一名称。路径、命令和代码标识符保持原样。引用实现时写明文件路径和符号名。

领域正文区分当前实现与已批准目标。尚未实现的目标说明已知差距。迁移顺序、进度和执行结果放入任务记录。设计理由只保留已知取舍；原资料没有记录时，不补写历史比较结论。

## 验证与提交

按 [定向验证](docs/testing.md#定向验证) 选择检查。具体环境、真实 Provider 证据和视觉检查方法由测试指南维护。

提交标题沿用 `fix(scope): ...` 等现有格式。PR 描述说明问题、修改后的行为、本次检查结果和未验证范围。提交前检查 diff。漏洞报告使用 [私密渠道](SECURITY.md#私密报告漏洞)。
