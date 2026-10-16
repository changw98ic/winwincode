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
| `pnpm test:rust` | 在受管中间产物缓存中构建 helper 并执行测试 |
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

### 发布报告中的进程证据

production API 报告保存完整的 `artifacts.sourceSeal` 七字段和 `deviceWorkerLifecycle`。后者记录初始 Worker 被终止、新 Worker 完成任务，以及另一条活跃 Worker 跨 Server 重启存活后完成独立任务。受管 Worker 在任务完成后正常退出。进程身份同时绑定 PID 和启动身份。执行身份通过原始 dispatch authority 关联 Worker、Worker instance、ProductSession 和执行任务。启动进程的 WorkerSession 与执行任务的 canonical WorkerSession 分别保存。两次完成必须属于不同执行任务和 ProductSession。字段与发布门禁见 [发布指南](operations/release.md#构建和验证流程)。这些本机 fixture 检查不能单独证明远程两机部署。

## 文档验证

`pnpm verify:docs` 依次运行仓库格式检查、`tests/docs-check.test.mjs` 和 `scripts/check-docs.mjs`。检查器测试使用临时目录。这个命令不启动 Client、Rust、真实 Provider 或浏览器构建。

本地链接检查器支持常规内联链接、图片（含空描述图片），以及完整、折叠和快捷引用式链接。重复引用标签使用检查器能解析的第一条定义，遵循 [GFM 的首条定义规则](https://github.github.io/gfm/#example-173)。它忽略代码围栏和行内代码。它不访问外部链接，也不检查纯锚点。检查本地文件时，它会去掉 fragment，并解码 percent 编码。

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

浏览器测试的静态 Client 和产品 Server 使用本机 HTTP。Provider HTTPS 测试使用的证书由对应 fixture 生成。

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

CI 的 producer 和两个消费者使用固定的 cargo-nextest 0.9.143。消费者直接调用 `cargo-nextest`，不会使用本地编译或回退路径。

### CI 测试产物复用

`scripts/rust-test-archive.mjs produce` 构建 helper，并用 `cargo nextest archive --workspace --all-features --locked` 创建测试包。包中包括 helper 的测试和可执行文件。`.config/nextest.toml` 要求 helper 存在。nextest 收集测试二进制、非测试二进制、动态库和必要的构建输出；源码、生成的已跟踪文件与 fixture 来自同一 commit 的 checkout。

producer 用原来的 `--workspace --exclude winwincode-kernel-helper --all-features --locked` 和两个 `-E` 过滤器分别记录测试 ID。消费者对比实际 archive 的 ID，要求完全相等；helper 测试在两个消费者中另外保留。缺包、缺 helper、摘要不符、nextest 版本不符、OS/架构不符、commit 或 workflow run/attempt 不符时立即失败。

消费者用 `--workspace-remap` 和 target/build 目录重映射执行。运行时使用的源码目录与 `CARGO_BIN_EXE_*` 从环境变量读取。`CARGO_TARGET_DIR`、`WWC_WORKER_HELPER_EXECUTABLE` 和 Code Mode helper 都指向解包目录。禁止缺文件后重新编译。`tests/rust-test-archive.test.mjs` 的真实 Rust fixture 在旧 checkout 不可用且 `cargo`、`rustc` 被失败桩替代时验证这条路径。producer CI 通过 `WWC_NEXTEST_INTEGRATION=1` 运行它。

producer 通过 reusable-workflow outputs 传递实际 artifact ID、producer attempt 和 archive 摘要。消费者按 ID 下载，并校验这些成功依赖的输出，不猜测当前 consumer attempt 对应的产物；单独重跑失败消费者可复用成功 producer 的 archive。artifact 名称包含 run、producer attempt、SHA、OS 和架构，只供本次 workflow 使用，保留一天。它不是跨次构建缓存。Clippy 仍检查 all-targets/all-features；runtime 仍单独构建和验证默认 features 的产品。

### `test-support` 与 helper 前置条件

多个 crate 把测试专用端口和故障注入点放在 `test-support` feature 后面。相关测试目标在各自的 `Cargo.toml` 中用 `required-features = ["test-support"]` 声明。根测试脚本使用 `--all-features`，所以会启用这些目标。

手动运行定向 Rust 测试时，不要遗漏对应 feature。优先使用以下形态：

```text
cargo test --all-features --locked -p <package> --test <target>
```

Worker 集成测试依赖 `winwincode-kernel-helper`。`pnpm test:rust` 会先构建该二进制。直接运行 `pnpm test:rust:built` 或 Worker 定向测试前，必须先准备同一 `CARGO_TARGET_DIR` 下的 `debug/winwincode-kernel-helper`。CI 还把 `WWC_WORKER_HELPER_EXECUTABLE` 显式设为该路径。

### 外置 `CARGO_TARGET_DIR`

Rust 构建可以使用仓库外的绝对目录。构建、测试和后续产品验证必须使用同一个值。

```sh
export CARGO_TARGET_DIR=/tmp/winwincode-target
pnpm test:rust
```

`scripts/test-rust-workspace.mjs`、`scripts/build-community.mjs`、`scripts/run-api-production-vertical.mjs` 和 `scripts/verify-products.mjs` 都读取该变量。不要在同一目标目录中并行运行会覆盖同一二进制的任务。`verify:api-production-sessions` 要求会话验证目录与当前配置的 `CARGO_TARGET_DIR/debug` 是同一 realpath。

### 本地 Rust 中间产物缓存

`scripts/rust-build-cache.py` 使用 Rust 1.95 的 `CARGO_BUILD_BUILD_DIR`，把中间产物放到项目自己的 `.cache/rust-build-v1/<debug|release>/<兼容摘要>/build`。`build:rust`、`lint:rust`、`test:rust`、`test:rust:built` 和 `test:rust:archive` 及 API 生产链路的 Cargo 构建使用该入口。直接运行 Cargo 不会自动纳入管理。需要 Python 3 和 POSIX `flock`；支持的开发宿主仍为 Linux/macOS。

兼容摘要包含项目 realpath、OS/架构、Rust/Cargo 版本、profile、feature 变体、命令参数、Cargo 配置与锁文件、Rust/Cargo 编译环境和显式 C/C++ 编译器版本。Cargo 继续负责桶内的依赖与源码新鲜度判断。debug 和 release 的桶独立；默认 features、all-features 与 Clippy 也独立。常规 `target/debug`、`target/release` 或显式 `CARGO_TARGET_DIR` 仍接收产品输出。

每个桶使用内核独占锁。进入构建时记录实际选择的桶，结束后更新时间，再扫描并整桶删除连续 86400 秒未使用的受管缓存。命中构建即使没有重写产物也会更新时间；失败构建同样收尾，清理或时间更新失败不会覆盖构建退出码。其他活跃构建持锁的桶会被跳过；等待构建与清理使用相同的锁。锁文件保留，避免删除再创建锁导致两个进程锁住不同 inode。

访问粒度是一次 Cargo 构建选择的整个兼容桶，不是桶内每个 Cargo 文件。清理不使用 mtime，不逐个删除 Cargo 内部文件，也不扫描源码、用户数据、其他项目缓存或发布制品。新桶在专用暂存目录完成所有权记录后原子发布。首次写入失败不会留下最终桶；中断遗留的未发布目录不会被当作可复用桶，不明暂存数据保留。缺少合法所有权记录、symlink 根目录或异常元数据时保留或拒绝，目录内部的 symlink 不会被递归跟随。已存在的传统 `target` 不会被接管或清理。没有定时任务。

显式设置 `CARGO_BUILD_BUILD_DIR` 表示目录由调用方管理。`WWC_RUST_CACHE=off` 关闭本地缓存管理；发布制品门禁明确使用此值，保留同一输出目录的两次刻意清空重建。不要并行构建会覆盖同一产品输出路径的任务；需要隔离时设置不同的 `CARGO_TARGET_DIR`。

### 远端缓存的限制与信任边界

GitHub 临时 runner 沿用常规 `target`，不恢复整个 Cargo target。现有本地快照的 target 已达 GiB 量级，且测试包不含全部 rlib、fingerprint 与 V8 对象；不能把 archive 的大小当作完整 target 的传输成本。当前最小方案是同次测试 archive，加跨次 sccache 和 V8 `gn_out`。CI 输出 archive 大小及缓存传输日志，待有实际冷/热对照后再决定是否加入跨次 target 缓存。没有声称获得未测量的性能收益。

sccache 根据工具链、编译参数和输入生成缓存键，不缓存链接等不支持的调用。GHA 后端没有暴露每个编译对象的完整使用记录或 24 小时 TTL。GitHub cache 的命中与访问时间属于整个远端条目；本地脚本不能实施逐对象访问清理。当前 workflow 只有 `contents: read`，不增加缓存删除或凭据权限；远端保留与驱逐由服务管理。24 小时策略适用于上述本地受管桶，不能宣称它控制了远端服务。

主线 sccache 命名空间包含 OS、架构、debug 与 PR/可信运行边界；V8 键还包含 lane、锁文件、配置、构建脚本和 Clang 身份。PR 使用 PR 专属命名空间，不通过宽泛 restore-key 把不可信 PR 产物送入可信构建。发布流程保持自己的缓存与干净构建约束。

历史 job `116576278491` 的 378 次 sccache 写入错误只有统计，未记录后端原因。`SCCACHE_LOG=sccache::server=debug` 在新构建中采集诊断，`scripts/report-sccache-errors.mjs` 只输出错误类别、HTTP 状态和计数，随后删除原始日志。不得根据命中率推断配额或权限故障。

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

## 远程 Device 验证

需要 Ubuntu 22.04 x86_64 验收制品时，在同一源码提交上手动运行 `Native Code Mode acceptance`，将 `remote_artifacts` 设为 `true`。该模式只运行 Ubuntu 22.04 构建任务，使用已锁定的 Node.js 24、Rust 1.95.0 和 LLVM 23。默认值为 `false`，沿用原生 Kernel 验证任务。

下载名为 `remote-test-support-ubuntu-22.04-x86_64-<commit>` 的 artifact。保存期为 7 天。压缩包只包含 `winwincode-server`、`wwc`、`winwincode-worker`、`winwincode-kernel-helper` 和 `winwincode-api-production.source.json`。来源封印绑定干净提交和四个程序。该包启用 `test-support`，仅用于隔离验收，不是发布包。解压后仍须按下文验证来源封印和两端源码，不能仅凭 CI 构建成功声明两机验收通过。

远程验证覆盖绑定、身份恢复、HTTPS/WSS、取消、断线重连、吊销和候选应用恢复。先准备相同 `CARGO_TARGET_DIR` 下的内核辅助进程，再选择相关检查：

```sh
cargo test --all-features --locked -p winwincode-device-client --lib remote_auth::tests::
cargo test --all-features --locked -p winwincode-device-client --lib http::
cargo test --all-features --locked -p winwincode-device-client --lib preview::tests::authentication_refreshes_one_challenge_and_revocation_stops
cargo test --all-features --locked -p winwincode-server --lib device_auth::tests::
cargo test --all-features --locked -p winwincode-server --test device_auth
cargo test --all-features --locked -p winwincode-storage --lib device_auth::tests::
cargo test --all-features --locked -p winwincode-device-client --test apply_engine
```

Device 的绑定测试使用本机 HTTP fixture 丢弃成功响应。重开数据目录后，它核对原请求、原私钥、原意图和原 Server URL。TLS 测试运行真实 TLS listener，分别检查显式 CA 成功、未知 CA 拒绝和主机名不匹配拒绝。Server 和 Storage 测试核对挑战单次消费、绑定事务原子性、幂等回执和永久吊销。挑战容量 fixture 覆盖匿名单公钥和多公钥占满、冒用已绑定公钥不落库、并发请求独立消费、同一挑战并发重放拒绝、大量已有消费记录下持续认证，以及过期回收。这些检查使用 fixture，不能代替真实两机证据。

配对拒绝回归使用真实 Server HTTP 路由，分别验证错误令牌和过期令牌失败后，可在同一 Device 目录提供新令牌完成绑定。Device 测试验证普通认证失败保留原 pending 请求，普通 `401` 和非吊销 `403` 后重启仍会签名请求。明确吊销会永久拒绝。成功响应丢失和保存回执前子进程崩溃时，即使提供新令牌也恢复原绑定。

仅启用 `test-support` 时，`WWC_DEVICE_TEST_EXIT_BEFORE_BIND_RECEIPT_SAVE=1` 会让 `device bind` 在成功响应校验完成、保存本地回执之前以状态 `86` 退出。随后移除该变量，使用相同数据目录重跑绑定。进程级验收须核对 Server 已有一个绑定、本地仍保存原 pending 请求、恢复后返回原回执。这验证客户端收成功响应后崩溃的恢复。

真实 TCP 丢响应使用隔离验收 Server 的 `test-support` 构建。启动时设置 `WWC_TEST_DROP_FIRST_DEVICE_BIND_RESPONSE=1`。该进程会在首次成功绑定提交后截断响应流，并保留数据库中的原回执。stderr 输出 `winwincode-server: test-support dropped committed device binding response`。同一进程只注入一次。`tests/device_auth.rs::committed_binding_survives_real_response_transport_failure` 核对真实连接失败、原回执恢复和唯一绑定。该网络故障与 Device 的 `exit(86)` 是两个独立故障点。

候选应用的 `WWC_TEST_APPLICATION_CRASH_POINT=after_started` 和 `after_repository_commit` 也只用于 `test-support`。前者验证 journal 已落盘而仓库未写入时的恢复。后者验证仓库提交已产生而完成回执尚未保存时的恢复。报告必须同时保存审批、基线、应用 ID、提交尾注和当前 epoch 的 reconciled 结果。

两机 runner 重启 Device 后，先等待 Server 确认当前实例，再核对两端的占用租约、持有者、epoch 和镜像 revision。就绪后才发送应用命令。崩溃场景同时要求 API 返回 `504 APPLY_RESULT_TIMEOUT`、Device 退出码为 `86`，以及对应应用的 journal 和故障标记。其他 API 结果保留在失败报告中。

应用恢复场景通过正常占用释放和重新申请建立新 epoch。runner 先恢复 Device，再调用 `DELETE /api/v1/clients/occupancy`，使用 `mode: release`。占用变为 `available` 后重新申请，并等待新租约和镜像一致。

真实两机验收须使用独立的 Server 主机和 Device 主机。验收至少保存以下证据：

- 两端精确源码版本、二进制摘要、对外 HTTPS 地址与证书校验结果。
- 首次绑定与响应丢失后的同一绑定回执、Server 公钥记录和 Device 稳定请求 ID。
- 已绑定 Device 重启后的签名交换与任务执行结果。
- 在线取消、断网后租约到期、重连后的旧执行围栏和 Worker 真实退出身份。
- 常规关闭与突发 `SIGKILL` 分别记录主进程、后代和工具独立进程组的退出。完整退出结论同时核对这些身份。
- `worker-crash-process-closure` 记录 Worker 崩溃后的受管工具进程关闭。`device.toolTty` 选择本次实际验证的 pipe 或 PTY 变体。
- 在线吊销与离线吊销后的停机、明确失败原因和恢复连接后的拒绝结果。报告同时保存 Server 吊销前后的关联 Job、lease terminal、admission reservation 与 reconciliation、原 Worker 和 Worker instance 的 active slot 事实。证据须覆盖旧 attempt 的 active slot，确认原 Worker 不再获得新任务，并确认已有终态不变。用量无法确认时，证据保留 pending reconciliation、unknown 用量和已知下界。
- 候选应用前后崩溃的 journal、Git 事实和当前授权下的恢复裁决。

两机 runner 是 `scripts/run-remote-device-fault-acceptance.mjs`。两端需要 Node.js 24、Git、相同源码快照和已构建的产品程序。Server 主机运行该脚本，并通过已配置 host key 的 SSH 访问 Device。Device 使用独立的验收目录和验收仓库。脚本会启动和终止验收进程，并写入该仓库。

先生成待执行计划：

```sh
node scripts/run-remote-device-fault-acceptance.mjs --plan --output /private/remote-acceptance-plan.json
```

计划报告保留所有项目为 pending，并以状态 `2` 退出。正式执行使用权限为 `0600` 的普通 JSON 配置文件：

```sh
node scripts/run-remote-device-fault-acceptance.mjs --config /private/remote-acceptance.json --output /private/remote-acceptance-report.json
```

配置的 `schemaVersion` 为 `1`。`server` 指定 HTTPS `url`、浏览器 `origin`、`tlsMode`、Owner `username`、密码环境变量名 `passwordEnvironment`、本端只读 `database` 路径、实际运行的 Server `binary` 绝对路径与 `pid`，并可指定 `caFile`。真实 TCP 丢响应还需要 `logFile`，以核对本次新增的截断响应 marker。`tlsMode` 使用 `direct` 或 `loopback-proxy`；后者还需要回环 HTTP `backendUrl`。`device` 指定 `sshHost`、源码目录 `checkout`、程序目录 `binaryDirectory` 和新建的、位于源码目录外的独立验收目录 `directory`。使用指定 SSH 私钥时，将本机私钥的绝对路径写入 `device.identityFile`。`device.controlPath` 可指定本机 SSH master 控制 socket 的绝对路径。该 master 应已为同一 `sshHost` 完成 host key 校验。runner 通过 `-S <controlPath>` 复用连接，并保留 `StrictHostKeyChecking=yes`。

`binaryDirectory` 和 Server 程序所在目录都必须包含四个产品程序及 `winwincode-api-production.source.json`。两端通过 `verifyApiProductionSourceSeal` 将完整制品与当前源码绑定。`device.toolTty` 可设为 `true` 或 `false`；一次运行只声明实际执行的 PTY 或 pipe 变体。

故障钩子需要启用 `test-support` 的 Server 与 Device 构建，并设置配置中的 `testSupport: true`。离线场景还需要 `networkFault.disconnect`、`networkFault.reconnect` 命令参数数组和 `preservesSsh: true`。这些命令应只阻断 Device 到验收 Server 的业务连接，并保留 SSH。`maxOfflineWaitMillis` 默认是 `120000`。实际租约期限超出等待预算时，离线场景保持 pending。完整校验规则由脚本的 `validateRemoteFaultConfiguration` 维护。

runner 核对两端操作系统主机身份、源码 commit、API 来源摘要、runner/fixture 文件摘要，以及本端实际 Server 进程映射。报告还保存十个执行依赖的源码摘要和 `acceptanceLogicSha256`。报告的模型证据标为 `fixture`。每个场景独立记录 passed、failed 或 pending。缺少网络注入或未实际命中故障钩子的场景保留 pending；整体报告为 incomplete，并以状态 `2` 退出。实际运行失败返回 `1`，所有必需场景通过才返回 `0`。

实际运行前，各项状态应为 pending。真实两机验收结论必须依据本次报告中实际运行的场景组合判定。计划报告、单机 fixture、单元测试和历史报告不能替代本次两机报告。涉及编程任务质量时，还必须按下一节使用真实 Provider。

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
| `rust-test-build` | 构建 helper 和完整 nextest archive，记录原过滤器的测试 ID |
| `rust-tests/test` | 下载本次 archive，运行原标准测试过滤器和 helper 测试 |
| `rust-tests/e2e` | 下载本次 archive，运行原重负载过滤器和 helper 测试 |
| `rust/runtime` | 构建 Rust 产品，检查产物，并运行两个 API 生产链路验证 |
| `verify` | 要求 source、typescript、Rust 构建、producer 和两个测试消费者成功 |

本地定向验证用于快速反馈。主线 CI 是合入前的完整组合门禁。

## 常见失败

| 现象 | 处理 |
| --- | --- |
| `canonical TypeScript test is missing: <路径>` | 恢复文件，或同步 `canonicalTestFiles` |
| `Chrome or Chromium is required ...` | 安装浏览器，或设置 `CHROME_BIN` |
| helper 路径报 `ENOENT` | 使用 `pnpm test:rust`，或先在同一 `CARGO_TARGET_DIR` 构建 helper |
| `generated contract drift` | 运行 `pnpm contracts:generate`，审查并提交全部生成产物 |
| API 会话验证的 source seal 失败 | 在同一 checkout 和目标目录先运行 `verify:api-production-vertical` |
| 未安装 cargo-nextest | 本地脚本会回退到 `cargo test`；CI 仍使用 cargo-nextest |
