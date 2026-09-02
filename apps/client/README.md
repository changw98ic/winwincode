# @winwincode/client

WinWinCode 的独立浏览器应用包。它提供一个应用壳，默认进入 Chat，并统一放置
StrongFlow、设置、审批和企业管理入口。所有 HTTP 与 WebSocket 请求都通过
`ControlPlaneClient`，并从部署时注入的同一个 `serverUrl` 派生。

## 构建

```bash
corepack pnpm --filter @winwincode/client build
```

可直接部署的静态文件位于 `dist/public/`。该命令只编译浏览器代码，不构建 Server、
Worker 或 Rust 程序。

部署时用实际配置覆盖 `dist/public/runtime-config.js` 中的空 `serverUrl`。前端代码和
其余静态文件不需要重新编译。例如：

```js
globalThis.__WINWINCODE_CLIENT_CONFIG__ = Object.freeze({
  serverUrl: 'https://control.example',
})
```

`asset-manifest.json` 记录每个不可变静态文件的大小和 SHA-256；可在部署时覆盖的
`runtime-config.js` 单独列出，不参与资产摘要。其中四个发布目标引用完全相同的浏览器
资产。`version.json` 记录产品版本和 Control Plane 合同版本。
