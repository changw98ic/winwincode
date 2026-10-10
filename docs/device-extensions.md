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

Core 请求工具许可时，页面通过现有审批列表显示 MCP 服务、请求摘要和 `mcp_permission` 原因。批准只对该次请求生效，拒绝会回传 Core。需要填写数据的表单和 URL 登录请求目前不能通过普通审批代填或批准；可以拒绝以结束等待。

## 配置生效与保存

设备将配置保存在私有 SQLite 数据库中。Web 使用所选设备的公钥加密配置；Server 校验管理权限并转发密文，只保留技能元数据、MCP 工具名、版本和操作回执。MCP 环境变量、请求头及技能正文不进入 Server 的公开状态。

启用、停用、修改或删除后，**同一聊天的下一次任务生效**。Worker 在新任务开始前读取设备最新配置，清除旧技能文件和技能缓存，同时更新 Core 的 MCP 配置与 Worker 工具授权清单。正在执行的任务保持本次配置，完成后切换；恢复中断任务时也保留该任务的配置。MCP 调用仍须经过 Worker 的已发现工具清单和 Action Gateway；页面保存成功不会跳过执行权限检查。 聊天中的后续任务使用独立的执行序号，服务端的聊天运行修订号持续递增；历史回执仍绑定原任务。

管理接口为 `GET/POST /api/v1/clients/{clientId}/extensions`；回执为 `GET /api/v1/clients/{clientId}/extensions/receipts/{requestId}`。设备公开元数据合计最多 192 KiB，以确保报告能通过设备消息通道。配置操作使用设备版本号和请求 ID 防止过期覆盖、篡改重试及重复执行。

可运行检查：`cargo test -p winwincode-provider --test device_extensions`。测试覆盖旧 Provider 数据迁移、WebCrypto、回放、同 Worker 配置刷新、任务恢复、资源导入，以及真实 stdio / HTTP MCP 握手。

## Device Provider 请求协议与私有请求头

设备的模型设置支持 `anthropic_messages`、`openai_chat_completions`、
`openai_responses` 和 `canonical`。API 地址必须填写所选协议的完整 HTTPS
请求地址；设备不会改写路径。
模型名使用服务商的 API 标识。

“自定义请求头”输入 JSON 对象，例如 `{"x-opencode-session":"<session>"}`。
留空保留设备已有值，填写 `{}` 清除。请求头与 API Key 一起加密发送到设备，
仅存储在设备的私有数据库中，服务器配置查询不返回名称或值。请求头不能覆盖
认证、目标主机、内容类型、请求身份或连接控制字段。

MiMo `mimo-v2.6-pro` 的原生 CodeMode 工具使用 `openai_responses`。标准 API
地址为 `https://api.xiaomimimo.com/v1/responses`；中国区 Token Plan 地址为
`https://token-plan-cn.xiaomimimo.com/v1/responses`。自定义请求头填写：

```json
{"x-openai-internal-codex-responses-lite":"true"}
```

此请求头显式启用 MiMo Responses Lite。工具保留原生 custom 输入和 Lark
语法。配置依据见服务商的[Codex 配置说明](https://mimo.mi.com/docs/en-US/tokenplan/integration/codex-configuration)
与 [Responses API 参考](https://mimo.mi.com/docs/en-US/api/chat/responses)。

MiMo 在“结构化输出格式”中显式选择“文本（本地 JSON 校验）”，对应
`responsesStructuredOutput: "text"`。需要结构化结果时，Device Provider 将
完整的原 JSON Schema 加入指令，并省略请求中的 `text` 字段，使用 API 默认
文本格式。最终结果仍须解析为 JSON，并通过原 schema 的本地严格校验。
工具清单、custom 输入和 Lark 语法保持原样。

此设置仅用于 `openai_responses`。省略配置时仍使用 `json_schema`；已有
`json_object` 配置继续请求服务端 JSON 对象格式。设备不会根据服务商名称、
模型名或请求头自动切换格式。`text` 和 `json_object` 只接受已实现的 schema
子集；未知关键词在发送模型请求前拒绝。

设备数据库从版本 2 升级到 3 时，在事务中增加 `provider_headers` 表，保留
已有 Provider、密钥和设备身份。升级后只运行当前版本；不要用旧程序打开新库。
