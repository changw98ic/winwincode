# ADR-0021：受管命令只能经过原生沙箱、DSH 凭据边界和脱敏安全审计

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.5.3`
- 实现：[`crates/kernel/src/governed_command.rs`](../../crates/kernel/src/governed_command.rs)、[`packages/native/src/index.ts`](../../packages/native/src/index.ts)、[`packages/strongflow/src/governed-process-executor.ts`](../../packages/strongflow/src/governed-process-executor.ts)、[`packages/strongflow/src/security-audit.ts`](../../packages/strongflow/src/security-audit.ts)、[`packages/dsh-profile/src/model-port.ts`](../../packages/dsh-profile/src/model-port.ts)
- 测试：[`tests/native-governed-command.test.mjs`](../../tests/native-governed-command.test.mjs)、[`tests/strongflow-governed-process-executor.test.mjs`](../../tests/strongflow-governed-process-executor.test.mjs)、[`tests/strongflow-security-audit.test.mjs`](../../tests/strongflow-security-audit.test.mjs)、[`tests/dsh-model-port.test.mjs`](../../tests/dsh-model-port.test.mjs)、[`tests/fixtures/installed-native-smoke.mjs`](../../tests/fixtures/installed-native-smoke.mjs)

## 结论

模型不能直接创建子进程。`command.run` 和 `test.run` 先由 StrongFlow 核对一份来自可信流程数据的准确授权；工具、作业、阶段、尝试、角色、上下文、内核会话、参数和工作目录必须全部相同。没有授权、授权器报错或任一字段不同，都在调用原生模块前按“流程权限拒绝”结束。

通过核对的命令只交给内嵌 Codex 的平台沙箱：

- macOS 使用 Seatbelt；
- Linux 使用打包的 seccomp/Landlock helper；
- 缺少对应后端或 Linux helper 时返回 `ENFORCEMENT_UNAVAILABLE`；
- 不会降级到普通 `spawn`，也不会改走外部 Codex CLI 或其他 Agent。

原生接口只接收 `sessionId`，角色、工作区和可用工具从已经接受的内核会话中读取。普通聊天会话不能调用这个接口，受管会话也不能通过 fork 产生一条没有原权限信封的新路径。

## 固定执行顺序

```text
模型的 command.run / test.run
              │
              ▼
宿主严格检查工具参数、真实工作区路径和敏感文件
              │
              ▼
可信授权器查找完全相同的计划命令 / 验证探针 / 修复命令
       │没有或不同
       └──────────────▶ POLICY_DENIED，不创建进程
              │相同
              ▼
先写安全审计：授权身份、参数摘要、环境变量名称、限制值
              │
              ▼
原生内核按 sessionId 取回不可修改的角色权限
              │
              ▼
清空环境变量 + 建立隔离 HOME/TMP + 生成最小文件权限
              │
              ▼
Seatbelt 或 seccomp/Landlock 启动进程组，网络保持关闭
              │
              ▼
有上限地收集输出；超时、取消或超量时终止整个进程组
              │
              ▼
安全审计只写输出 SHA-256 和字节数；脱敏结果才返回模型
```

## 文件与进程边界

命令必须使用绝对可执行文件路径，内核会先解析真实文件。它不会启动登录 shell、读取 shell profile 或继承调用者环境。显式 shell 也只是一条需要完全匹配授权的普通命令，并继续处于同一个操作系统沙箱和进程组中。

文件权限只包含：

1. Codex 运行所需的最小系统只读路径；
2. 当前角色被分配的唯一工作区；
3. 当前命令独占的临时目录；
4. 当前可执行文件。

Executor 和 Remediator 可以写候选工作区，其他可运行验证探针的角色只能读。`.git`、`.agents` 和 `.codex` 在可写工作区内仍是只读。以下凭据敏感路径无论角色是否可写都拒绝读取和写入：`.env`、`.env.*`、`.credentials.yaml`、`.netrc`、`.npmrc`、`.pypirc`、常见私钥与证书文件、SSH 私钥和 Docker 凭据文件。TypeScript 工具边界也使用同一份路径清单，明确路径在进入文件工具前就会被拒绝；整库搜索和差异执行器必须保留排除清单。

## 网络与远程副作用

受管命令固定使用 `NetworkSandboxPolicy::Restricted`。当前实现没有“批准后临时改成全网”的第二条路径，因此本地监听地址、互联网地址和远程写入都不能建立连接。需要网络提升时只能产生带来源身份的人工请求；在专门的目的地址策略落地前，即使有人作出决定也不会把本命令路径改成可联网。

每项命令都有最长十分钟的时间上限和最多 8 MiB 的合并输出上限。取消、关闭会话或关闭内核会通知所有活动命令，并终止进程组，避免子进程在角色结束后继续运行。

## 环境变量与 DSH 凭据

进程创建前执行 `env_clear()`。内核自行生成 `PATH`、隔离的 `HOME`、`TMPDIR`、`TMP`、`TEMP`、`CI` 和 `NO_COLOR`；可信授权只能另外提供格式受限的 `LANG`、`LC_ALL`。任何其他名称，包括 Token、认证头、代理、动态加载和语言运行时注入变量，都会在创建进程前被拒绝。

模型调用继续只经过 DSH `ctx.llm`。WinWinCode 只传 provider/model 等模型调用字段，不读取 DSH 凭据服务；具体适配器在实际 `stream()` 调用时解析凭据引用。模型桥现在还会：

- 拒绝 DSH 返回的多余或疑似凭据配置字段；
- 不复制提供商的原始错误消息；
- 保留经过格式检查的错误类别、状态码和重试时间；
- 只保留提供商请求编号的 SHA-256，不保留原值。

因此 DSH 原始密钥不会进入 StrongFlow 角色环境、Codex 错误事件或持久化安全记录。

## 持久化安全审计

`DurableStrongFlowSecurityAudit` 按作业建立独立目录，目录名是作业编号的 SHA-256，不把原始编号直接用作路径。`security.jsonl` 使用 `0600` 权限；每条记录包含连续序号、前一条摘要、本条事件和本条摘要。追加前会取得跨进程排他锁并重新验证整条摘要链，写入后同步文件和目录。读取时也会重新验证，修改、删除中间记录或交换顺序都会返回 `AUDIT_CORRUPT`。

安全事件固定记录作业、阶段、尝试、角色、上下文、内核会话沿袭、实际会话、事件流、事件序号、回合和操作编号。审批、工具、进程、会话接受和凭据边界共用这一条记录流。命令输出只保存 SHA-256 和字节数，不保存 stdout/stderr。字段名、Bearer 值、环境赋值、JWT、私钥块和登记的敏感值会在持久化前统一隐藏。

## 失败分类

以下结果不会再合并成同一种“工具失败”：

| 分类 | 含义 | 例子 |
| --- | --- | --- |
| `POLICY_DENIED` | 请求没有获得流程授权 | 无准确命令授权、工具不属于角色、工作目录或环境超出范围 |
| `SANDBOX_DENIED` | 已授权命令被操作系统边界拒绝 | 越界写入、敏感文件读取、网络连接 |
| `TOOL_FAILED` | 权限允许，但任务本身失败 | 测试返回非零、超时、取消、输出超限 |

这三类结果分别进入角色运行结果和安全审计。界面或控制器不需要解析操作系统错误文字来猜测是否越权。

## 四个平台的证据

本地 Node 测试会在当前主机直接验证工作区读写、越界写入、敏感文件、网络、空环境、超时、取消、只读角色和普通聊天拒绝。发布工作流 [`native-release.yml`](../../.github/workflows/native-release.yml) 在以下四个真实主机/处理器组合上重新构建发布包，并从空目录安装后运行同一组关键检查：

- `darwin/arm64`；
- `darwin/x64`；
- `linux/arm64`；
- `linux/x64`。

只有对应平台沙箱名称、网络拒绝、环境隔离、文件边界、敏感文件拒绝、超时和普通会话拒绝全部成立，`verify:native-install` 才通过。
