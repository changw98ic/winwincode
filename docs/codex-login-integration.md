# ChatGPT 授权与 Codex 登录态接入

## 直接授权

在设置页面选择一台已连接的设备。选择 `ChatGPT（直接授权）`，
点击 `Continue with ChatGPT`。WinWinCode 在所选设备上打开系统浏览器。
在浏览器中登录 ChatGPT，并允许 WinWinCode 使用 ChatGPT 计划。
回到设置页后等待“ChatGPT 授权已保存到设备”，再点击“测试连接”。

这条路径使用 OpenAI 的开源应用动态注册流程。每台设备使用稳定的主机标识。
每次登录生成独立的 PKCE、state 和 nonce。回调监听 `127.0.0.1`，
WinWinCode 验证 ID token 的签名、签发者、受众、有效期和 nonce。
首次注册返回的 client ID 与账号身份一同保存。重新授权必须保持同一账号。

凭据保存在设备私有数据库中，文件权限为 `0600`，目录权限为 `0700`。
设备在访问令牌过期前刷新凭据，并串行处理同一数据库中的刷新。
浏览器设置通道和 Server 只传递加密命令与公开回执。
令牌仅在设备的 Provider HTTP 边界使用，执行仍使用嵌入式 Codex Core。

授权后，设备读取该账号的可用模型目录。表单中的模型若不可用，
设备保存目录中的第一个可见模型。请求固定发往
`https://api.openai.com/v1/responses`，设置 `store=false` 和 `stream=true`。
连接测试会执行一次真实模型请求，并计入 ChatGPT 计划用量。

系统浏览器需要运行在所选设备上。当前入口适用于有图形环境的设备。
授权回调最长等待三分钟。拒绝授权、缺少计划使用权限或验证失败时，
设备保留已有凭据。切换账号时，添加另一个服务商 ID。
删除服务商会删除本机凭据。可在 ChatGPT 设置中管理 WinWinCode 的账号授权。

流程与账号资格见 [OpenAI 注册与登录文档](https://developers.openai.com/siwc/token-sharing-open-source/sign-in)
和 [ChatGPT 计划使用说明](https://developers.openai.com/siwc/token-sharing-open-source)。

开发阶段可以单独执行真实授权探针。该命令打开系统浏览器，
在指定设备目录保留凭据，并执行一次模型请求。目录必须是私有目录：

```bash
WWC_CHATGPT_LOGIN_DEVICE_DIR=<设备Provider目录> cargo test -p winwincode-provider \
  direct_chatgpt_authorization_live_probe --lib --locked -- --ignored
```

已有授权时，可以单独验证已保存的连接：

```bash
WWC_CHATGPT_LOGIN_DEVICE_DIR=<设备Provider目录> cargo test -p winwincode-provider \
  saved_chatgpt_connection_live_probe --lib --locked -- --ignored
```

## 复用 Codex 登录态

在设置页面选择已连接的设备。在服务商表单中选择
`Codex ChatGPT（设备登录态）`，填写服务商 ID、显示名称和模型 ID。
保存后点击“测试连接”。模型 ID 必须在该 ChatGPT 账号的权限范围内。
测试会调用一次真实模型，并使用该账号的 Codex 额度。

设备读取其启动环境中的 `CODEX_HOME/auth.json`。未设置 `CODEX_HOME` 时，
设备读取用户主目录下的 `.codex/auth.json`。浏览器操作的是所选设备的登录态。
远程设备需要先在该设备上完成 Codex 登录。

当前实现支持文件中的 ChatGPT 登录态。登录文件必须是私有普通文件，
权限不能向组或其他用户开放。设备拒绝过期令牌、符号链接和账号不一致。
系统钥匙串、API Key 登录和企业账号池需要各自的接入实现。

设备数据库保存登录目录和账号绑定。每次调用重新读取有效访问令牌。
Codex 管理登录和令牌刷新。WinWinCode 不更新 Codex 登录文件。
登录过期时，在 Codex 中重新登录同一账号后重试。
切换账号时，先删除该服务商，再重新添加。

请求固定发往 `https://chatgpt.com/backend-api/codex/responses`。
设备添加 OAuth Bearer 令牌和 `ChatGPT-Account-Id` 请求头。
配置和回执继续使用 Device 配置协议，执行继续使用嵌入式 Core。
响应保留工具调用、消息阶段和加密推理，以支持后续轮次。
令牌只在设备侧的 HTTP 调用中使用。

响应中的 Token 用量进入现有统计。上游没有返回实际费用时，金额保留为未知值。
订阅费、剩余额度和额外额度的购买价格需要各自的上游回执。
ChatGPT 登录和 API Key 登录的计费与策略范围见
[OpenAI 认证文档](https://learn.chatgpt.com/docs/auth)。

真实连接探针默认跳过。显式执行：

```bash
WWC_CODEX_TEST_MODEL=<可用模型ID> cargo test -p winwincode-provider \
  current_codex_login_live_probe --locked -- --ignored
```

该探针使用临时设备数据库，保存账号绑定并调用一次真实模型。
普通测试使用受控登录文件和响应，不消耗模型额度。
