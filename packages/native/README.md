# @winwincode/native

WinWinCode 内嵌 Codex Core 的 TypeScript 接口。它按当前 macOS 或 GNU Linux 主机加载对应的 N-API 原生库，不调用已安装的 Codex CLI，也不会回退到另一套 Agent 执行器。

## 构建

在仓库根目录执行：

```bash
corepack pnpm build:native
```

构建结果位于当前主机对应的平台包 `prebuild/` 中。四个包分别是：

- `@winwincode/native-darwin-arm64`；
- `@winwincode/native-darwin-x64`；
- `@winwincode/native-linux-arm64`；
- `@winwincode/native-linux-x64`。

每个平台包包括：

- `winwincode_native.node`：Node 原生模块；
- `winwincode-kernel-helper`：Codex 文件系统和子进程内部入口；
- `codex-linux-sandbox`：仅 Linux 提供，与 helper 是同一程序的固定名称入口；
- `build-info.json`：源码身份、工具链、补丁列表、文件大小和 SHA-256；
- `LICENSE`、`NOTICE`、`THIRD_PARTY_NOTICES.md`、`rust-dependencies.json` 和 `licenses/`：项目许可、第三方归属、目标专属依赖及其原始许可文件。

`@winwincode/native` 是小型加载器。安装时只会选择当前主机对应的平台包；Windows 或其他处理器会明确报错，不会选择相近产物，也不会回退到外部 Codex CLI。

## 使用

内核在第一次创建会话时严格读取 `home/config.toml`。文件不存在时使用 Codex 的默认只读权限；文件内容无效时返回 `CONFIG_LOAD_FAILED`，不会静默忽略。需要允许工具修改会话工作区时，可以在内核自己的数据目录中写入：

```toml
approval_policy = "on-request"
default_permissions = ":workspace"
```

会话的 `cwd` 必须是已经存在的绝对目录。内核会先解析符号链接和 macOS 的 `/var` 路径别名，再把同一个规范路径交给命令工作目录和沙箱，避免界面显示的目录与实际写权限不一致。

```ts
import { DshModelPort } from '@winwincode/dsh-profile'
import { WinWinCodeKernel } from '@winwincode/native'

const kernel = new WinWinCodeKernel({
  home: '/absolute/path/to/winwincode-home',
  modelPort: new DshModelPort(ctx.llm),
})

const session = await kernel.createSession({
  cwd: '/absolute/path/to/workspace',
  provider: 'deepseek',
  model: 'deepseek-chat',
})

const first = await kernel.pollEvent(session.sessionId, 1_000)
if (first.status === 'event') {
  console.log(first.event.sequence, first.event.kind, first.event.payload)
}

await kernel.closeSession(session.sessionId)
await kernel.shutdown()
```

`events()` 每个会话只允许一个活动订阅者。事件序号使用 `bigint`，不会把 Rust 的 64 位序号截短。`resolveApproval()` 使用事件里的会话、审批和 turn 身份，把人工决定交回同一个 Codex 回调。原生错误会转换为带稳定 `code` 的 `KernelError`。调用方在进程退出前应显式执行 `shutdown()`。

`@winwincode/dsh-profile` 提供 `CodexRuntimeProjector`、`DshRuntimeProjection` 和 `RuntimeApprovalRouter`。同一批原始事件无论实时送入还是从持久化记录重放，都会得到相同的聊天记录和界面状态。投影只保存展示副本，不会成为第二个执行内核。
