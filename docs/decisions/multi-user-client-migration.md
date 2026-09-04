# 多用户 Client 迁移边界 inventory（CLIENT-000.10）

- 状态：草案（Phase 0 迁移 inventory lane 交付）
- 日期：2026-09-04
- 输入：`winwincode-multi-user-client-complete-plan.md` §10.1、§22、Phase 1-8
- 机器可读清单：[`multi-user-client-migration.inventory.json`](multi-user-client-migration.inventory.json)
- 格式先例：[`0028-control-plane-worker-migration.inventory.json`](0028-control-plane-worker-migration.inventory.json)
- 行号基准：分支 `agent/muc-migration-inventory` 本次检出；后续演进时须重新核对

本清单盘点多用户共享 Client 迁移会触碰的既有触点。每个触点记录当前职责、
`file:line` 证据、计划中的迁移目标（Phase / Beads epic）与风险备注。
它描述"现状是什么、会被改成什么"，不引入新架构决定；目标架构见实施计划与后续 ADR。

## 触点总览

| id | 当前职责 | 迁移目标 | Phase / epic | 证据数 |
| --- | --- | --- | --- | --- |
| `server-fixed-auth-subject` | `WWC_SERVER_AUTH_SUBJECT` 提供唯一固定用户身份，注入 bootstrap actor、model authority、admission identity、enterprise identity | `UserAccountService` 持有的 UserAccount 身份 | Phase 1 / AUTH-100 | 10 |
| `server-single-repository-root` | `WWC_SERVER_REPOSITORY_ROOT` 把整个 Server 绑到一个本地 Git 仓库根 + 四个 scope ID | registered ClientNodes + `RepositoryBinding` | Phase 5 / REPO-100 | 6 |
| `bootstrap-proof-credential` | 一次性 bootstrap proof 换取 cookie 会话，digest 持久、write-only、不回显 | 未初始化 Server 的一次性初始化凭据，创建首个 Owner 后永久关闭 | Phase 1 / AUTH-100 | 22 |
| `remote-worker-exchange-endpoint` | `/internal/v1/execution-port/exchange`：文件凭据 Bearer 认证的 Remote Worker HTTPS exchange | 传输保留，新增 Worker Session Credential 与 LaunchGrant 验证 | Phase 6 / WORKER-200 | 12 |
| `worker-remote-entrypoint` | `winwincode-worker --remote`：全环境变量驱动的独立远程 Worker 进程 | 新增 `--managed-session <config-file>`（0600）受管启动方式 | Phase 6 / WORKER-100+200 | 5 |
| `browser-secure-cookie-session` | `wwc_session` HttpOnly/Secure/SameSite cookie + SQLite 会话：校验、缩权、撤销、重启存活 | 会话绑定 UserAccount：密码登录、TTL 续期、限流、禁用即撤销 | Phase 1 / AUTH-100 | 17 |
| `product-session-binding-entry` | Session 命令族建立 ProductSession / SessionBinding，精确身份 + replay + fencing | 提交前校验 occupied Client 与 RepositoryAccessGrant，SessionBinding 扩展本地执行来源 | Phase 7 / FLOW-100 | 13 |

合计 7 个触点、85 处 `file:line` 证据。

## 各触点摘要

### 1. `server-fixed-auth-subject` → Phase 1 / AUTH-100

`crates/winwincode-server/src/main.rs:139` 读取 `WWC_SERVER_AUTH_SUBJECT`，
并在四处独立消费：bootstrap actor（main.rs:143）、model authority（main.rs:208 与
645）、worker pool admission identity（main.rs:326）、enterprise identity 组装
（main.rs:526）。`crates/winwincode-server/README.md:26` 与三个垂直脚本
（`tests/browser-auth-session.test.mjs:169`、`tests/fixtures/real-browser-harness.mjs:212`、
`scripts/run-api-production-vertical.mjs:1021`）固化了该契约。

迁移风险：Plan Phase 1 任务 8 明确要求"迁移既有固定 subject"。最稳妥路径是
一次性迁移创建 userId 保持稳定的 Owner 账号，否则既有 audit/actor 事实的含义会
整体漂移；四个消费点必须同步切换，不能出现一半环境变量一半账号的中间态。

### 2. `server-single-repository-root` → Phase 5 / REPO-100

`crates/winwincode-server/src/main.rs:1090` 用 `WWC_SERVER_REPOSITORY_ROOT`
构造 `LocalDeliveryAdapterConfig`；`WWC_SERVER_SOURCE_ROOT` 也从它派生
（main.rs:123-130）。`crates/winwincode-server/tests/application_composition.rs:46`
用源码文本断言把该变量名钉死在 main.rs 里。

迁移风险（对应 Plan §22.2）：兼容期可把旧 repository root 映射为一个内置 Local
Client projection，但不能长期保留两套语义——这条必须写成有名字、有门禁的迁移
步骤。改名/删除时须同步更新 application_composition.rs 的源码文本断言。多仓库化
后，绝对路径归 Device Client 本地库，Server/公开投影只保留 RepositoryBinding
身份与安全元数据（public path ban）。

### 3. `bootstrap-proof-credential` → Phase 1 / AUTH-100

现状链路：环境变量 `WWC_SERVER_BOOTSTRAP_PROOF`（main.rs:138）→
`AuthSessionBootstrap`（`crates/winwincode-server/src/auth_session.rs:117`）→
`POST /api/v1/auth/session`（`crates/winwincode-server/src/server.rs:298`、
418）→ 换取独立随机 cookie（auth_session.rs:326、server.rs:449）。 secrecy
性质由单元测试钉住：digest 持久、不回显、重放即撤销（auth_session.rs:897）、
重启后仍可换（auth_session.rs:1155）。Client 侧表单与 Bearer 发送在
`apps/client/src/control-plane-client.ts:508`、656、`auth-page.ts:72`；HTTP 合同把
`bootstrapProof` 声明为 session 创建的 security scheme
（`tests/control-plane-http-contract.test.mjs:674`）；浏览器垂直断言 proof 不泄漏到
URL/DOM/storage/console（`tests/first-run-strongflow-browser.test.mjs:42`、
`tests/browser-chat-strongflow-production.test.mjs:64`）。

迁移目标（Plan §10.1）：Server 未初始化 → 浏览器输入 bootstrap proof → 创建首个
Owner → bootstrap 永久关闭。语义从"每次启动都可用的固定凭据"变为"整个生命
周期一次性"，auth_session.rs 的重启复用测试将被反向改写；用户名 + Argon2id
密码登录接管日常认证，但 write-only、不回显、digest 持久三条保证必须原样保留。

### 4. `remote-worker-exchange-endpoint` → Phase 6 / WORKER-200

路由注册在 `crates/winwincode-server/src/server.rs:308`，处理器
`remote_worker_exchange`（server.rs:319）校验 Bearer 凭据后委托
`ProductionRemoteWorkerExchange`（`crates/winwincode-server/src/remote_worker_transport.rs:264`），
凭据来自 `FileRemoteWorkerAuthenticator`（remote_worker_transport.rs:65，
main.rs:492 注入，`WWC_SERVER_REMOTE_WORKER_CREDENTIAL_FILE`）。Worker 侧
`RemoteWorkerPort::open`（`crates/winwincode-worker/src/remote_transport.rs:125`）
加载 TLS root 与私有凭据文件，POST 请求在 remote_transport.rs:286 组装。

迁移目标（Plan §22.3）：保留 RemoteWorkerPort 与 ExecutionPort 传输，不重写；
注册路径新增 Worker Session Credential 与 `WorkerLaunchGrant` 验证
（clientNodeId、occupancyLeaseId、fencing token、workerSessionId）。风险在于
当前身份只是一个静态文件 + 一组 issuer/subject/expiry 环境变量，admission
identity（main.rs:326）必须学会理解受管凭据，同时保持现有 ExecutionPort parity
测试全绿、错误分类稳定。

### 5. `worker-remote-entrypoint` → Phase 6 / WORKER-100 + WORKER-200

`crates/winwincode-worker/src/main.rs:32` 解析 `--remote`，`run_remote`
（main.rs:61）从 `WWC_WORKER_ID / INSTANCE_ID / DATA_DIRECTORY / SOURCE_ROOT /
SERVER_ORIGIN / TLS_ROOT_DER_FILE / CREDENTIAL_FILE` 等环境变量取全部配置。
`scripts/run-api-production-vertical.mjs:1093` 以 `['--remote']` 拉起 Worker。

迁移目标（Plan §14.4）：新增 `winwincode-worker --managed-session <config-file>`，
配置文件 0600，由 Device Client 写入 `clientNodeId / occupancyLeaseId /
occupancyFencingToken / repositoryBindingId / workerSessionId / sourceDirectory` 等；
`sourceDirectory` 只由 Device Client 写入。`--remote` 保留给 standalone 与测试，
两种入口必须共享同一执行内核，避免 managed 路径分叉执行语义。

### 6. `browser-secure-cookie-session` → Phase 1 / AUTH-100

`wwc_session` cookie 定义在 `crates/winwincode-server/src/auth_session.rs:30`，
`Set-Cookie` 属性 `HttpOnly; Secure; SameSite=None`（auth_session.rs:597，登出清除
在 628）。`SqliteAuthSessionManager`（auth_session.rs:147）提供签发、`current`
校验（484）、缩权（502）、单会话撤销（407）、按 actor 全撤销（535）；请求侧由
`server.rs:891` 的 authenticator 在 command/query/event 上强制执行；DELETE
`/api/v1/auth/session` 登出（server.rs:468）；授权变化时活动 WebSocket 先推送
撤销再关闭（`crates/winwincode-server/tests/server.rs:740`）。企业外部身份登录复用
同一 cookie 原语（`crates/winwincode-server/src/enterprise_identity_protocol.rs:318`）。

迁移目标（Plan §10.2）：会话绑定 UserAccount，新增 TTL 续期、登录限流、logout、
禁用用户即时撤销全部 Browser Session。`revoke_actor_sessions` 自然成为 disable
kill switch；cookie 属性与 secret-free 持久化保持不变。注意外部身份签发路径必须
一并收敛到 UserAccount，避免出现第二套会话语义。

### 7. `product-session-binding-entry` → Phase 7 / FLOW-100

入口链：`crates/winwincode-server/src/dispatcher.rs:32`（`SessionCreate` 归入
Session 命令族）→ `crates/winwincode-server/src/application.rs:425`（分发到
`ProductSessionApiService::create`）→ `crates/winwincode-control-plane/src/product_session_service.rs:657`
（create：revision 0、replay、scope、model-route 校验后建 ProductSession）→
turn 绑定 worker slot 时 `continue_session`（product_session_service.rs:710）持久化
`PersistedSessionBinding`（777）；`build_binding`（1605）经
`SessionBinding::pending` + `accept_worker_session`
（`crates/winwincode-session/src/binding.rs:303`、342）建立精确执行身份。Delivery
侧对应 `accept_worker_session_with_authority`
（`crates/winwincode-delivery/src/application/session_binding.rs:248`、
`crates/winwincode-delivery/src/store.rs:1639`）。

迁移目标（Plan §14.2/§14.3，Phase 7）：任务提交先过 occupied Client +
RepositoryAccessGrant + 容量校验，`ExecutionReservation` 增加
client/repository/occupancy 事实，`SessionBinding` 扩展本地执行来源。身份精确
匹配、replay receipt-first、stale fencing 拒绝三条现有规则冻结不动；扩展只能在
身份里加事实，不能放松任何现有 mismatch 拒绝路径。

## 迁移顺序建议

依赖关系遵循 Plan §24（CLIENT-000 → AUTH-100 / CLIENT-100 → …）：

1. **Phase 1 / AUTH-100**：`bootstrap-proof-credential` + `server-fixed-auth-subject`
   + `browser-secure-cookie-session`。先立 UserAccountService 与 Owner bootstrap，
   后续所有 grant、occupancy、audit 事实都要指向真实用户；这一步不做，后面的
   授权对象无从谈起。
2. **Phase 6 / WORKER-100 + WORKER-200**：`worker-remote-entrypoint` +
   `remote-worker-exchange-endpoint`。受管启动与 LaunchGrant 验证复用现有
   exchange 传输，可与单根调度并存，提前落地不阻塞。
3. **Phase 5 / REPO-100**：`server-single-repository-root`。Registry/ACL 依赖
   Phase 2-4 的 Device Client 本地路径存储；兼容窗口内用内置 Local Client
   projection 桥接旧配置。
4. **Phase 7 / FLOW-100**：`product-session-binding-entry`。占用与仓库授权检查
   需要 AUTH-100、CLIENT-300、REPO-100、WORKER-200 的事实全部就位后才接入
   session 创建路径；身份与 replay 规则在接入期间冻结。

发现的歧义（记录给后续 lane 求证）：

- `WWC_SERVER_SOURCE_ROOT` 目前从 repository root 派生（main.rs:123-130），
  多仓库后执行 source root 的来源（Device Client config？RepositoryBinding？）
  计划未明说。
- 固定 subject 一次性迁移时 Owner 账号的 userId 是否必须等于旧 subject 值，
  还是允许新建身份并映射历史 actor 事实，计划没有规定。
- Plan §22.2 的"内置 Local Client projection"是持久对象还是启动期适配器，
  与"不能长期保留两套语义"的边界判定标准需要 ADR 明确。
- `--managed-session` 与现有 `--remote` 的共存期长度未定义；vertical 脚本
  （run-api-production-vertical.mjs）何时切换到受管入口没有给出迁移门。
