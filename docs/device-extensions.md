# 在设备上使用 Skills 和 MCP

在 Web 的「扩展」页面选择已连接的设备。管理操作需要该设备的管理权限。

## 技能与指令

点击「添加技能」，填写标识，再选择一种导入方式：

- 填写**所选设备**上的绝对目录，目录中包含 `SKILL.md` 和配套资源。
- 直接粘贴完整的 `SKILL.md`。

`SKILL.md` 需要 `name`、`description` frontmatter。目录导入最多包含 256 个文件、1 MiB 数据和 12 层子目录；不接受符号链接，忽略 `.git` 与 `node_modules`。粘贴内容最多 32 KiB。导入只读取文件，不运行脚本。

保存并启用后，在同一聊天的下一条消息中使用 `$技能名称`。嵌入的 Codex Core 负责发现技能、加载指令和执行工具；脚本使用原有 Action Gateway 与沙箱权限。

## MCP 连接

点击「添加 MCP 服务」，填写服务标识和 JSON。例如：

```json
{
  "command": "node",
  "args": ["/absolute/path/to/server.js"],
  "env": { "SERVICE_TOKEN": "your-token" }
}
```

也支持 Streamable HTTP 的 `url`、`http_headers` 和环境变量引用。HTTP 地址支持 HTTPS，以及本机 `localhost` / `127.0.0.1` 的 HTTP 服务。

「保存并连接」先保存配置，再由设备启动服务，执行 MCP `initialize` 和 `tools/list`。页面显示设备确认的工具列表；连接失败会显示失败状态，失败或未测试的服务不会进入下一次任务的工具清单。

当前支持本地运行环境和显式认证配置。工具名使用字母、数字、下划线或短横线，服务标识与工具名合计最多 121 字节，每个服务最多 128 个工具。大小写冲突的标识、需要重命名的工具和 HTTP header helper 会被拒绝。需交互登录的 OAuth 服务目前没有 Web 登录流程。

## 配置生效与保存

设备将配置保存在私有 SQLite 数据库中。Web 使用所选设备的公钥加密配置；Server 校验管理权限并转发密文，只保留技能元数据、MCP 工具名、版本和操作回执。MCP 环境变量、请求头及技能正文不进入 Server 的公开状态。

启用、停用、修改或删除后，**同一聊天的下一次任务生效**。Worker 在新任务开始前读取设备最新配置，清除旧技能文件和技能缓存，同时更新 Core 的 MCP 配置与 Worker 工具授权清单。正在执行的任务保持本次配置，完成后切换；恢复中断任务时也保留该任务的配置。MCP 调用仍须经过 Worker 的已发现工具清单和 Action Gateway；页面保存成功不会跳过执行权限检查。 聊天中的后续任务使用独立的执行序号，服务端的聊天运行修订号持续递增；历史回执仍绑定原任务。

管理接口为 `GET/POST /api/v1/clients/{clientId}/extensions`；回执为 `GET /api/v1/clients/{clientId}/extensions/receipts/{requestId}`。设备公开元数据合计最多 192 KiB，以确保报告能通过设备消息通道。配置操作使用设备版本号和请求 ID 防止过期覆盖、篡改重试及重复执行。

可运行检查：`cargo test -p winwincode-provider --test device_extensions`。测试覆盖旧 Provider 数据迁移、WebCrypto、回放、同 Worker 配置刷新、任务恢复、资源导入，以及真实 stdio / HTTP MCP 握手。
