# UI-106 独立验收 notes（winwincode-os0）

- 日期：2026-09-02
- 审查员工作目录：`/Volumes/ORICO/winwincode-worktrees/cde-glm-review-ui106`（本 worktree）
- 实现（只读）：`/Volumes/ORICO/winwincode-worktrees/ui-workbench-xhigh`
- 结论：**有条件通过（PASS with 2 non-blocking findings）**
- 已知排除：winwincode-os0.1 的真实 auth fixture Server 启动参数问题未计入本验收。

## 逐项验收结论

| 验收项 | 结论 | 证据 |
| --- | --- | --- |
| HTTP/WS/auth/permission/version/offline 统一分类 | 通过 | `connection-state.ts` 单一 `ControlPlaneClientError.kind` → `ClientFailureCategory` → `GlobalConnectionStatus`；HTTP（observe 的 command/query/restore）与 WS（onEvent/onResetRequired/onAuthorizationRevoked/onError）都汇聚到同一 monitor。Node 测试 `client-global-reliability.test.mjs` 5/5 |
| 七态 Connection Bar | 通过 | `PRESENTATION` 覆盖 connected/reconnecting/offline/refresh-required/authentication-required/permission-denied/version-mismatch，各有 tone/live/recovery；真实 Chrome `client-reliability-browser.test.mjs` 逐一断言 |
| 重连与草稿保留 | 通过 | 真实 Chrome：offline→draft 保留 `draft-provider`，online→reconnectAll() 重连计数 >0，authentication-required→草稿仍在；Connection Bar 文案明确 "unsaved fields remain" |
| route/订阅清理 | 通过 | `performRender()` 每次 render 先 abort featureController + close activeFeature；`observeControlPlaneClient` 包装 close 一并清理订阅集合；`client.close()` 移除全部 5 个 window 监听 + 2 个订阅 + 全部子组件。浏览器测试断言 settingsSubscriptionClosed=true、abortedQueries=['settings.get'] |
| ARIA/键盘/窄屏 | 通过 | Connection Bar `aria-label=Server connection`，badge role=status/alert + aria-live polite/assertive 按 severities 分级；error boundary `role=alert`；按钮为原生 `<button>`（Enter/Space 可达）；`base.css` 全局 `:focus-visible` 3px 实线 outline（Chrome 实测 outlineStyle=solid/outlineWidth=3px）；`components.css` `@media (max-width: 48rem)` 收敛 connection actions；360px 实测无横向溢出 |
| secret-safe 诊断 | 通过（见 finding 1 的保真度例外） | `createSafeDiagnostic` 只输出 7 行固定字段；code 经 PUBLIC_CODES 允许列表、requestId 经 `^req_[0-9A-HJKMNP-TV-Z]{26}$`、scope 只留 kind + 尾 6 位缩写、时间戳经 RFC3339+Date.parse 双验证。对抗性输入（恶意 code/surface/scope/时间戳/换行注入）实测全部被净化 |
| 敏感信息不进 DOM/日志/诊断 | 通过 | 见下"敏感信息专项搜索" |

## 敏感信息专项搜索（重点）

搜索范围：`apps/client/src`（排除 generated）。

1. `innerHTML/outerHTML/insertAdjacentHTML/document.write`：**0 处**。所有 UI 文本经 `textContent`。
2. `console.*`：**0 处**。
3. `error.message` 直出 DOM 的路径共 3 处，全部安全：
   - `chat-page.ts:251` / `strongflow-page.ts:129`：仅当 `error.code.startsWith('STRONGFLOW_CREATE_')`；这些码只由客户端 `clientFailure()` 以静态字面量产生（脚本验证无插值/动态参数），服务端错误码是封闭 12 值枚举（`ErrorCode`），不可能以该前缀出现。
   - `strongflow-page.ts:111`：仅当 `error.code.startsWith('STRONGFLOW_')`，同样全部是客户端静态文案。
   - 服务端 message 即使到达也已被双保险：`public_message()` 把 INTERNAL/APPLICATION 错误替换为固定文案并限长 4096；`CredentialLeakGate` 在 HTTP/WS 出口对 code+message 做指纹与编码检查，命中即整体替换为 `CREDENTIAL_OUTPUT_REJECTED` 固定文案。
4. 各页面错误文案（settings/local-operations/local-decisions/enterprise/auth）：全部走 kind/code → 固定文案映射，无原始 message 透出。浏览器测试断言 `doesNotMatch /private route fixture/`。
5. `ErrorDetails` 的 key 在 generated client 里有 `CANONICAL_FORBIDDEN_ERROR_KEYS` + 禁用片段递归检查，越界即整体降级 `INVALID_RESPONSE`。
6. boot.ts 启动失败：只输出固定文案 + `CLIENT_STARTUP_FAILURE` 码（diff 移除了原先的 `error.message` 透出——本 PR 明确改进了这一点）。
7. 模型原文/工具载荷：诊断结构里根本没有承载 message/payload 的字段；scope 摘要只含 kind + 尾 6 位。

## 定向验证运行（全部在候选 worktree）

- `corepack pnpm exec tsc -p apps/client/tsconfig.ui-components-tests.json`：0 错误
- `corepack pnpm typecheck`（全仓库）：通过
- Node 测试：client-global-reliability 5/5、client-ui-components 14/14、client-ui-styles 3/3、settings+local-decisions+local-operations 15/15、generated-control-plane-client 48/48、chat-page+chat-view-model 34/34、strongflow-page+view-model 37/37、chat-control-plane-integration 2/2、keyed-collection+canonical-api-contract+http/ws-contract 31/31、enterprise 页面 6/6
- 真实 Chrome 测试：client-reliability-browser ✔、chat-empty-browser ✔、chat-to-strongflow-browser ✔、strongflow-empty-browser ✔、enterprise-management-pages-browser ✔、client-feature-routes-browser ✔（但见 finding 2）

## Findings（不阻塞验收）

1. **winwincode-xdd**（低）：`connection-state.ts:448` 传 `PERMISSION_REVOKED`，不在 `PUBLIC_CODES`，被改写为 `CLIENT_FAILURE`。状态正确、无泄密，仅诊断码保真度受损。复现：`review/ui106/verify-permission-revoked-code.mjs`。修复建议：加入允许列表或改用 `PERMISSION_DENIED`。
2. **winwincode-e6q**（低，测试侧）：`tests/client-feature-routes-browser.test.mjs:88-94` 竞态，实测约 5% 失败率；隔离复现 `review/ui106/verify-feature-routes-race.mjs`（7/120）；最小修复 `review/ui106/fix-feature-routes-race.patch`。产品代码无缺陷。

## 审查工件（本 worktree）

- `review/ui106/verify-permission-revoked-code.mjs` — finding 1 最小复现
- `review/ui106/verify-feature-routes-race.mjs` — finding 2 隔离竞态复现（真实 Chrome）
- `review/ui106/fix-feature-routes-race.patch` — finding 2 最小修复说明
- `review/ui106/fix-permission-revoked-code.md` — finding 1 修复建议
