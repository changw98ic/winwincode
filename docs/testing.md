# 测试

本文档说明本地测试、定向验证和主线门禁。契约测试见 [契约](contracts.md)。系统边界见 [架构总览](architecture/overview.md)。前端结构见 [前端架构](architecture/frontend.md)。长期约束见 [不变量](invariants.md)。发布前检查见 [发布](operations/release.md)。贡献流程见 [CONTRIBUTING.md](../CONTRIBUTING.md)。

## 测试层级

| 层级 | 主要位置 | 标准入口 |
| --- | --- | --- |
| 文档格式与本地链接 | 根 Markdown、`docs/`、`scripts/check-docs.mjs`、`tests/docs-check.test.mjs` | `pnpm verify:docs` |
| 源码格式与边界 | `scripts/check-format.mjs`、`scripts/lint-workspace.mjs`、`scripts/check-network-entrypoints.mjs` | `pnpm verify:source` |
| TypeScript 构建与测试 | `scripts/run-ts-tests.mjs`、`tests/*.test.mjs` | `pnpm test:ts` |
| Rust 单元与集成测试 | `crates/*/src`、`crates/*/tests`、根测试目标 | `pnpm test:rust` |
| Rust 重负载端到端测试 | `production_vertical`、`worker_lifecycle`、`code_mode_kernel` | `WWC_RUST_E2E=1 pnpm test:rust` |
| 产品运行时验证 | `scripts/run-api-production-vertical.mjs`、`scripts/run-api-production-sessions.mjs` | `pnpm verify:api-production-vertical`、`pnpm verify:api-production-sessions` |
| 契约一致性 | `schema/`、`scripts/generate-contracts.mjs`、`scripts/generate-network-policy.mjs` | `pnpm contracts:check` |

## 根脚本

根 `package.json` 是命令事实来源。

| 命令 | 行为 |
| --- | --- |
| `pnpm test` | 依次运行 `pnpm test:ts` 和 `pnpm test:rust` |
| `pnpm test:ts` | 先运行 `pnpm build:ts`，再运行 `node scripts/run-ts-tests.mjs` |
| `pnpm test:rust` | 先构建 `winwincode-kernel-helper`，再运行 `pnpm test:rust:built` |
| `pnpm test:rust:built` | 运行 `node scripts/test-rust-workspace.mjs`。调用前必须已有 helper |
| `pnpm verify:docs` | 运行仓库格式检查、文档检查器的隔离测试和本地文件链接检查。它不构建产品 |
| `pnpm verify:source` | 先运行 `pnpm verify:docs`，再检查 Rust 格式和源码边界 |
| `pnpm verify:typescript` | 运行 `pnpm test:ts`，再检查 TypeScript 产品产物 |
| `pnpm verify:rust` | 构建、lint、测试 Rust，并运行产品和 API 生产链路验证 |
| `pnpm verify` | 依次运行 source、TypeScript 和 Rust 完整门禁 |
| `pnpm typecheck` | 先运行 `pnpm contracts:check`，再运行 `tsc -p tsconfig.check.json` |
| `pnpm lint:rust` | 运行 `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` |
| `pnpm build:ts` | 检查契约，构建 TypeScript 项目，再构建 Client |
| `pnpm build:rust` | 运行 `scripts/build-community.mjs` 构建产品二进制 |

`preinstall` 会运行 `scripts/check-runtime.mjs`。本仓库要求 Node.js 24.x。支持的宿主是 macOS 或 Linux 的 arm64、x64。

### API 链路验证的证据范围

`scripts/run-api-production-vertical.mjs` 默认使用 `deterministicDeviceProvider()`，并启动本地确定性模型服务。发布工作流和本地制品门禁沿用这个 Device Provider fixture。

这些检查运行真实 Server、Device Client、Worker 和内核二进制。它们验证提交、取消、StrongFlow、重启恢复和消息字节稳定性。模型响应来自 fixture，所以它们不能单独证明真实 Provider 的编程任务质量。

`pnpm verify` 包含上述确定性链路验证。真实编程任务验收应另行使用正式 Client、Device 和真实 Provider 执行，并保存实际模型、调用事实、候选改动和独立验证证据。Provider 配置步骤见 [部署指南](operations/deploy.md)。

## 文档验证

`pnpm verify:docs` 依次运行仓库格式检查、`tests/docs-check.test.mjs` 和 `scripts/check-docs.mjs`。检查器测试使用临时目录。这个命令不启动 Client、Rust、真实 Provider 或浏览器构建。

本地链接检查器支持常规内联链接，以及完整、折叠和快捷引用式链接。它忽略代码围栏和行内代码。它不访问外部链接，也不检查纯锚点。检查本地文件时，它会去掉 fragment，并解码 percent 编码。

链接和引用定义须位于单行，标签不支持嵌套方括号。普通目标不支持空白或右括号；带空白的目标使用尖括号。本地目标中的查询字符串按文件名处理。检查器不实现完整 Markdown 解析。复杂语法、全部标题锚点、外部链接有效性和文档语义仍需人工核对。`pnpm verify:docs` 通过只证明它覆盖的格式、测试和本地文件目标通过。

## TypeScript 测试

`scripts/run-ts-tests.mjs` 维护 `canonicalTestFiles`。脚本先确认每个清单文件都存在，然后用以下参数串行运行：

```text
node --experimental-test-module-mocks --test --test-concurrency=1 <canonicalTestFiles>
```

`pnpm test:ts` 只运行这个清单。新增 `tests/*.test.mjs` 时，必须判断它是否属于必跑测试。属于必跑测试时，要把路径加入 `canonicalTestFiles`。

TypeScript 测试使用 Node.js 内置的 `node:test` 和 `node:assert/strict`。页面级组件测试可使用 JSDOM。`tests/chat-page.test.mjs` 和 `tests/strongflow-review-ui.test.mjs` 展示了当前 JSDOM 入口。专用 `apps/client/tsconfig.*-tests.json` 会把被测 TypeScript 编译到 `.cache`，测试再加载编译产物。

### 真实浏览器入口

`tests/fixtures/real-browser-harness.mjs` 是浏览器测试入口。它通过 Chrome DevTools Protocol 驱动真实 Chrome 或 Chromium。它按以下顺序寻找浏览器：

1. `CHROME_BIN`
2. macOS 的 Google Chrome 或 Chromium 应用路径
3. `PATH` 中的 `google-chrome` 或 `chromium`

浏览器测试还需要 `openssl`。测试用它生成只在本次测试中使用的本地证书。

当前 `canonicalTestFiles` 包含以下真实浏览器测试：

- `tests/home-dashboard-browser.test.mjs`
- `tests/readiness-browser.test.mjs`
- `tests/strongflow-review-browser.test.mjs`
- `tests/ui604-shell-a11y-browser.test.mjs`
- `tests/usage-health-browser.test.mjs`

这些测试从 `apps/client/dist` 和各浏览器包的 `dist` 读取产物。测试夹具通常提供受控的控制平面响应。`tests/browser-chat-production.test.mjs` 等生产浏览器测试还会启动真实 Server 和内嵌 Kernel。它们不在 `scripts/run-ts-tests.mjs` 的固定清单中，必须按任务要求单独运行。

### 共享 Client 构建目录

`apps/client/scripts/build.mjs` 会先删除再重建 `apps/client/dist`。脚本用 `apps/client/.client-build.lock` 串行化构建动作。这个锁不隔离正在读取 `dist` 的测试或服务。

不要并行运行会重建 Client 的浏览器测试。多个验证任务并行时，应使用隔离的 checkout 或隔离的输出目录。需要交给用户继续验收的服务必须保持其对应 `dist` 不变。

交付仍供用户验收的服务时，记录地址、数据目录和启动信息。服务保持运行，直到用户明确要求停止或确认不再需要。

## Rust 测试

`scripts/test-rust-workspace.mjs` 先编译并运行 `winwincode-kernel-helper` 的测试。它随后准备可执行的 debug helper。

如果已安装 cargo-nextest，脚本运行：

```text
cargo nextest run --all-features --locked --workspace --exclude winwincode-kernel-helper
```

如果未安装 cargo-nextest，脚本回退到 `cargo test`。两条路径都使用 `--all-features` 和 `--locked`。

`WWC_RUST_E2E` 控制重负载测试：

- `WWC_RUST_E2E=0` 排除 `production_vertical`、`worker_lifecycle` 和 `code_mode_kernel`。
- `WWC_RUST_E2E=1` 只运行这三个目标。
- 未设置时运行全部目标。

CI 的 `test` 和 `e2e` lane 安装固定版本的 cargo-nextest。CI 因此不会使用本地回退路径。

### `test-support` 与 helper 前置条件

多个 crate 把测试专用端口和故障注入点放在 `test-support` feature 后面。相关测试目标在各自的 `Cargo.toml` 中用 `required-features = ["test-support"]` 声明。根测试脚本使用 `--all-features`，所以会启用这些目标。

手动运行定向 Rust 测试时，不要遗漏对应 feature。优先使用以下形态：

```text
cargo test --all-features --locked -p <package> --test <target>
```

Worker 集成测试依赖 `winwincode-kernel-helper`。`pnpm test:rust` 会先构建该二进制，并由 `scripts/compact-kernel-helper.mjs` 处理 64 MiB 的认证镜像上限。直接运行 `pnpm test:rust:built` 或 Worker 定向测试前，必须先准备同一 `CARGO_TARGET_DIR` 下的 `debug/winwincode-kernel-helper`。CI 还把 `WWC_WORKER_HELPER_EXECUTABLE` 显式设为该路径。

### 外置 `CARGO_TARGET_DIR`

Rust 构建可以使用仓库外的绝对目录。构建、测试和后续产品验证必须使用同一个值。

```sh
export CARGO_TARGET_DIR=/tmp/winwincode-target
pnpm test:rust
```

`scripts/test-rust-workspace.mjs`、`scripts/build-community.mjs`、`scripts/run-api-production-vertical.mjs` 和 `scripts/verify-products.mjs` 都读取该变量。不要在同一目标目录中并行运行会覆盖同一二进制的任务。`verify:api-production-sessions` 要求会话验证目录与当前配置的 `CARGO_TARGET_DIR/debug` 是同一 realpath。

## 定向验证

开发中的验证应覆盖受影响边界。不要为局部改动重复运行无关的完整门禁。

| 变更 | 最小验证 |
| --- | --- |
| 纯文档 | `pnpm verify:docs`，再人工核对标题锚点、外部链接、命令和源码事实。通常不需要重跑完整 Rust 测试 |
| JSON Schema 或生成器 | `pnpm contracts:generate`，再运行 `pnpm contracts:check` 和相关契约测试 |
| TypeScript 视图模型或页面 | 对应 `tsconfig.*-tests.json`、对应 `tests/*.test.mjs`；交互变化再运行对应真实浏览器测试 |
| Rust crate | `cargo test --all-features --locked -p <package>`；跨 crate 边界时增加相关集成测试 |
| Worker 或 Kernel 执行路径 | 先准备 helper，再运行对应 Worker 或 Kernel 目标 |
| HTTP、WebSocket 或产品组合 | 相关契约测试，加对应生产链路验证 |

单个 Node.js 测试应保留规范运行参数：

```text
node --experimental-test-module-mocks --test --test-concurrency=1 tests/<name>.test.mjs
```

定向验证通过后，再由主线门禁覆盖完整组合。

## 真实任务与 fixture

真实编程任务的产品验收必须使用真实 Provider。验收证据应绑定实际 Provider 请求、模型响应、工具执行、候选变更和验证结果。模型自报成功、单个命令退出码或历史测试结果不能单独证明本次任务通过。

确定性 fixture 用于组件测试、协议测试、错误分支和可重复对照。fixture 产生的报告、数据集和截图必须明确标记为 `fixture` 或 `synthetic`。不要把 fixture 结果标记为生产任务或真实 Provider 结果。

真实 Provider 凭据只通过任务规定的环境或凭据存储提供。凭据、Provider 原始私有负载和运行数据库不得进入提交。

## 视觉验收

视觉变更必须在真实浏览器中生成真实截图。评审者应把截图与设计稿逐屏对照，并覆盖目标视口和关键状态。

保留的 `ui608` 测试记录 DOM、布局和样式指纹，已移出规范 TypeScript 测试清单，只作参考。它不能替代像素截图和设计评审。不要把 `tests/fixtures/visual-regression/*.baseline.json` 当作视觉验收基线。

`tests/browser-chat-production.test.mjs` 展示了 `Page.captureScreenshot` 和 PNG 产物的实现。新视觉验收应按具体页面保存可审查截图，并记录视口、状态和设计稿来源。

## 主线 CI

`.github/workflows/mainline.yml` 在 pull request 和推送到 `main` 时运行完整门禁。

| job | 内容 |
| --- | --- |
| `source` | `corepack pnpm verify:source` |
| `typescript` | 安装锁定依赖，再运行 `corepack pnpm verify:typescript` |
| `rust/lint` | 安装 Linux 构建依赖，再运行 `corepack pnpm lint:rust` |
| `rust/test` | `WWC_RUST_E2E=0` 下运行 `corepack pnpm test:rust` |
| `rust/e2e` | `WWC_RUST_E2E=1` 下运行 `corepack pnpm test:rust` |
| `rust/runtime` | 构建 Rust 产品，检查产物，并运行两个 API 生产链路验证 |
| `verify` | 要求 source、typescript 和全部 rust matrix lane 成功 |

本地定向验证用于快速反馈。主线 CI 是合入前的完整组合门禁。

## 常见失败

| 现象 | 处理 |
| --- | --- |
| `canonical TypeScript test is missing: <路径>` | 恢复文件，或同步 `canonicalTestFiles` |
| `Chrome or Chromium is required ...` | 安装浏览器，或设置 `CHROME_BIN` |
| helper 路径报 `ENOENT` | 使用 `pnpm test:rust`，或先在同一 `CARGO_TARGET_DIR` 构建 helper |
| `Kernel helper exceeds the authenticated image limit` | 检查 release helper 的体积和 `scripts/compact-kernel-helper.mjs` |
| `generated contract drift` | 运行 `pnpm contracts:generate`，审查并提交全部生成产物 |
| API 会话验证的 source seal 失败 | 在同一 checkout 和目标目录先运行 `verify:api-production-vertical` |
| 未安装 cargo-nextest | 本地脚本会回退到 `cargo test`；CI 仍使用 cargo-nextest |
