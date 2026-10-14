# 前端架构

本文描述 WinWinCode 0.2.0 的浏览器前端。它同时记录当前实现和 React 迁移约定。整体架构见[架构总览](overview.md)。作用域模型见[作用域模型 ADR](../adr/0004-scope-model.md)。

相关文档：

- [文档索引](../README.md)
- [测试](../testing.md)
- [契约](../contracts.md)
- [React UI ADR](../adr/0002-react-ui.md)

## 产品范围

前端面向单用户、单工作区、多项目的产品目标。项目在浏览器中以仓库作用域打开。一个登录会话可以访问多个项目和仓库。

当前实现仍使用完整的组织、工作区、项目和仓库标识。服务端通过 `authorizedScopes` 返回授权作用域。浏览器从 URL 读取作用域，并把它解析为一个仓库作用域。多个兼容作用域会进入选择状态。无兼容作用域或未授权作用域会进入空状态或拒绝状态。因此，0.2.0 尚未在类型和路由层强制单工作区。

前端负责展示、交互、浏览器路由和临时 UI 状态。服务端负责认证、授权、持久状态和执行。前端不能用隐藏控件或只读提示代替服务端授权。

依据：`packages/contracts/src/browser-control.ts` 中的 `ControlPlaneAuthSession`、`Scope` 和 `RepositoryScope`；`packages/browser-core/src/scope-context.ts` 中的 `resolveScopeContext`；`apps/client/src/application.ts` 中的 `authenticatedRouteContext`。

## 启动与挂载

`apps/client/public/index.html` 提供唯一的应用根节点 `data-winwincode-client-root`。页面先加载可部署时修改的 `runtime-config.js`，再加载 `assets/client.js`。

`apps/client/src/boot.ts` 是浏览器启动入口。它读取根节点和运行时配置，然后调用 `mountWinWinCodeClient`。根节点缺失会抛出错误。运行时配置读取失败时，启动入口会渲染带 `role="alert"` 的错误消息。

运行时配置位于 `globalThis.__WINWINCODE_CLIENT_CONFIG__`。当前唯一字段是 `serverUrl`。`readWinWinCodeClientRuntimeConfig` 要求它是绝对 HTTP 或 HTTPS URL。URL 不能包含凭据、查询串或片段。部署可以修改 `runtime-config.js`，无需重新构建静态资源。

`mountWinWinCodeClient` 是组合根。它完成以下工作：

1. 挂载主题偏好和应用外壳。
2. 创建 Control Plane 客户端、连接监视器和查询缓存。
3. 创建认证、登录、设备、仓库和就绪状态等长期视图模型。
4. 挂载导航、连接状态、登录和错误边界。
5. 根据当前 hash 挂载功能页面。
6. 订阅 `hashchange`、在线状态、全局错误和认证状态。
7. 恢复登录会话。

功能切换会先中止未完成的加载，再关闭旧页面。路由页面和视图模型使用动态 `import()` 按需加载。异步挂载使用渲染代次检查，避免旧请求覆盖新路由。`close()` 会移除事件监听器、停止定时器、关闭视图模型和客户端，并清空根节点。

依据：`apps/client/public/index.html`；`apps/client/src/boot.ts` 中的 `rootElement` 和 `renderStartupFailure`；`apps/client/src/runtime-config.ts` 中的 `readWinWinCodeClientRuntimeConfig`；`apps/client/src/application.ts` 中的 `mountWinWinCodeClient`、`launchRoute` 和 `render`。

## Hash 路由

前端使用浏览器 hash。它没有引入独立路由库。`clientSurfaceFromHash` 去掉 `#` 和查询字符串，再按路径前缀选择页面。未知路径回退到默认的 `#/chat`。

当前一级页面如下：

| Hash | 页面 | 导航入口 |
| --- | --- | --- |
| `#/chat` | 新对话和已有会话 | 有 |
| `#/home` | 任务看板 | 有 |
| `#/projects` | 项目和仓库 | 有 |
| `#/extensions` | 扩展 | 有 |
| `#/device` | 执行设备 | 无 |
| `#/settings` | 设置 | 有 |
| `#/onboarding` | 首次设置 | 无 |

`#/home` 还有当前子路由：

| Hash | 功能 |
| --- | --- |
| `#/home/new-task` | 创建任务 |
| `#/home/run`、`#/home/task-run` | 任务运行 |
| `#/home/preview` | 候选运行预览 |
| `#/home/clarify` | 决策澄清 |
| `#/home/review` | StrongFlow 评审 |

其他状态放在查询参数中。例如，聊天使用 `session`、`client` 和 `repositoryBinding`。作用域使用 `organizationId`、`workspaceId`、`projectId` 和 `repositoryId`。`surfaceHash` 和 `scopeHash` 在页面跳转时保留作用域参数。

每次渲染都会重新计算页面能力和作用域。能力可以是可编辑、只读、禁用或隐藏。路由授权被撤销后，前端关闭当前功能，清除当前作用域，并显示安全入口。

依据：`apps/client/src/client-surface.ts` 中的 `CLIENT_SURFACES` 和 `clientSurfaceFromHash`；`apps/client/src/application.ts` 中的 `homeSubRoute`、`performRender`、`renderChat` 和 `onRouteAuthorizationRevoked`；`packages/browser-core/src/scope-context.ts` 中的 `scopeSelectionFromHash`、`scopeHash` 和 `surfaceHash`。

## 渲染模型

0.2.0 是混合前端。

应用外壳和当前路由页面使用命令式 DOM。页面通常由 `mount*Page` 或 `render*Page` 创建节点。挂载结果提供 `close()`。可复用控件还可以提供 `update()`。`@winwincode/browser-ui` 也使用这种挂载协议。聊天等页面会在自己的 DOM 中预留根节点，再挂载 React 岛。

React 负责局部岛。`apps/client/src/dsh-ui.tsx` 用 `createRoot` 挂载以下功能：

- `MarkdownText`：聊天 Markdown。
- `AttachmentRail`：待发送附件。
- `SessionBrowser`：历史会话列表。

`mountChatMarkdown`、`mountAttachmentRail` 和 `mountSessionBrowser` 是命令式页面与 React 岛之间的适配层。调用方只依赖 `update()` 和 `close()`。React 根不会接管外壳或整个页面。

依据：`apps/client/src/application.ts` 中的 `element` 和 `mountWinWinCodeClient`；`packages/browser-ui/src/mounted-view.ts` 中的 `MountedView`；`apps/client/src/dsh-ui.tsx` 中的 `mountChatMarkdown`、`mountAttachmentRail` 和 `mountSessionBrowser`。

## 状态、视图模型和 Client API

功能状态位于视图模型。视图模型公开只读 `state`、`subscribe()`、用户命令和 `close()`。`subscribe()` 会立即发送当前状态，并返回退订函数。状态更新会发布新的不可变对象。页面订阅状态，并把用户操作转成视图模型方法调用。

视图模型使用 `AbortController` 取消在途请求。异步加载使用代次检查丢弃过期结果。实时订阅处理增量事件、重置请求和授权撤销。写命令携带预期修订号，并核对响应的请求标识和命令名。异步读取、命令提交、实时订阅、取消和错误归一化应留在视图模型或 Client API 适配器中。

当前组合根仍直接执行少量跨页面编排。例如，它读取会话详情和历史会话，解析路由上下文，并创建功能专用端口。这些调用属于应用组合层。功能组件不应复制 Control Plane 请求结构。

Client API 的调用链如下：

```text
页面或 React 组件
  → 功能视图模型或应用组合层
  → 功能端口 / ControlPlaneClient
  → 查询缓存和连接观察器
  → 生成的 HTTP / WebSocket transport
  → Control Plane
```

`community-control-plane-client.ts` 是前端的产品适配入口。它组合通用 Control Plane 客户端、生成的契约和功能端口。当前协议版本是 `winwincode/v1`。命令使用 `POST /api/v1/commands`。查询使用 `POST /api/v1/queries`。实时事件使用 `/api/v1/events` 的 WebSocket 订阅。

`apps/client/src/generated/contracts.ts` 提供产品请求、响应和投影类型。`apps/client/src/generated/control-plane-client.ts` 提供生成的 HTTP 与 WebSocket transport。生成输入来自 `schema/winwincode/v1`。这些文件不应手工修改。

`createQueryCache` 包装查询客户端。缓存键包含 actor、scope、查询名、参数和分页。相同键的并发查询共享在途请求。命令、实时事件、授权纪元变化和重连会按相应作用域失效或清空缓存。连接观察器把网络、授权和版本错误映射到统一连接状态。

依据：`apps/client/src/chat-view-model.ts` 中的 `ChatViewModel` 和 `createChatViewModel`；`apps/client/src/settings-view-model.ts` 中的 `SettingsViewModel` 和 `createSettingsViewModel`；`apps/client/src/community-control-plane-client.ts`；`apps/client/src/generated/contracts.ts`；`apps/client/src/generated/control-plane-client.ts`；`apps/client/src/core/connection-state.ts` 中的 `observeControlPlaneClient`；`packages/browser-core/src/query-cache.ts` 中的 `createQueryCache` 和 `queryCacheKey`。

## React 迁移约定

目标 UI 使用 TypeScript 和 React 函数组件。迁移按页面或局部功能进行。迁移必须保留现有视图模型、Client API 边界、hash 路由和用户可见行为。迁移不新增路由依赖或全局状态依赖。

React 组件使用 `useSyncExternalStore` 订阅现有视图模型。当前视图模型的监听器接收最新状态，但还没有统一的 `getSnapshot()` 方法。适配代码应从 `model.state` 读取快照，并把现有监听器转换成 React 的无参数变更通知：

```tsx
import { useCallback, useSyncExternalStore } from 'react'

function useViewModelState<State>(model: {
  readonly state: State
  subscribe(listener: (state: State) => void): () => void
}): State {
  const subscribe = useCallback(
    (onStoreChange: () => void) => model.subscribe(() => onStoreChange()),
    [model],
  )
  const getSnapshot = useCallback(() => model.state, [model])
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot)
}
```

视图模型必须在状态变化时发布新的不可变快照。一个快照在下一次发布前必须保持引用稳定。组件卸载时，React 会调用 `subscribe()` 返回的退订函数。组件事件处理器只调用视图模型命令或明确注入的端口。

这项约定尚未全量实现。0.2.0 中，`CodeBlock` 使用 `useSyncExternalStore` 订阅语法高亮语法包的加载状态。页面视图模型尚未全部接入该 Hook。多数页面仍通过命令式 `subscribe(state => render(state))` 更新 DOM。

依据：`apps/client/src/dsh/markdown/CodeBlock.tsx` 中的 `CodeBlock`；`apps/client/src/chat-page.ts` 中的 `mountChatPage`；`apps/client/src/chat-view-model.ts` 中的 `subscribe`；[React UI ADR](../adr/0002-react-ui.md)。

## 共享包边界

前端相关的六个共享包各自承担一个稳定边界。

| 包 | 边界 |
| --- | --- |
| `@winwincode/browser-core` | 浏览器查询缓存和作用域路由。它不包含产品页面。 |
| `@winwincode/browser-ui` | 产品中立的 DOM 控件和挂载协议。它提供按钮、面板、状态标记、页面标题、空状态和错误状态。 |
| `@winwincode/contracts` | 主机、页面和浏览器控制面的共享 TypeScript 契约。它定义标识、actor、scope、命令、查询和 WebSocket 帧。 |
| `@winwincode/control-plane-client` | 产品中立的浏览器 Control Plane 门面。它注入生成的 transport，并统一认证、HTTP、WebSocket 和错误处理。 |
| `@winwincode/network-request` | 网络失败分类、重试、回放与对账决策、取消和截止时间。Control Plane 客户端通过它执行请求。 |
| `@winwincode/strongflow` | StrongFlow 页面描述符和凭据边界。前端的评审页面仍位于 `apps/client`。 |

应用代码通过包的公开导出使用这些能力。它不应跨包引用 `src` 内部文件。产品专用契约由代码生成后放在 `apps/client/src/generated`，再由产品适配层交给通用客户端。

依据：六个包各自的 `package.json`；`packages/browser-core/package.json` 的公开子路径配置；`packages/browser-ui/src/index.ts`；`packages/contracts/src/index.ts` 和 `packages/contracts/src/browser-control.ts`；`packages/control-plane-client/src/index.ts` 中的 `createControlPlaneClient`；`packages/network-request/src/index.mjs` 中的 `executeRequest`；`packages/strongflow/src/index.ts` 中的 `strongFlowSurface`。

## 样式和 DSH 来源

`apps/client/src/styles/client.css` 是样式入口。它按 tokens、base、shell、components 和 features 的顺序建立 CSS layer。功能样式不得绕过语义 token 直接复制颜色和间距。

Token 有两层：

- `dsh-tokens.css` 保存适配后的 DSH 静态色板和别名。
- `tokens.css` 把 DSH 别名映射为 WinWinCode 的 `--wwc-*` 语义 token，并定义间距、字体、圆角、层级和尺寸。

`dsh-theme.css` 为聊天 Markdown 和代码高亮提供局部桥接。附件组件使用 `dsh-attachment.css`。浅色和深色由 `<html>` 的 `data-theme` 选择。`mountPreferences` 从 `localStorage` 读取 `light`、`dark` 或 `system` 偏好，并更新该属性。功能样式使用 `--wwc-*`。适配后的 DSH 组件内部可以使用 `--dsw-*`。

DSH 代码来自 DeepSeek Harness `dsh-v0.1.0-rc.8`，提交 `141eb6fef83422698aef7a981029e843e8161534`。`upstream/dsh-frontend.json` 记录源文件、目标文件、上游 SHA-256 和本地适配。`THIRD_PARTY_NOTICES.md` 记录 MIT 条款。当前适配包括 Markdown、代码块、附件栏、会话列表的部分行为、主题 token 和若干样式片段。

依据：`apps/client/src/styles/client.css`、`apps/client/src/styles/dsh-tokens.css`、`apps/client/src/styles/tokens.css`、`apps/client/src/styles/features/dsh-theme.css` 和 `apps/client/src/styles/features/dsh-attachment.css`；`apps/client/src/preferences.ts` 中的 `mountPreferences`；`upstream/dsh-frontend.json`；`THIRD_PARTY_NOTICES.md`。

## 构建和产物

工作区要求 Node.js 24 和 pnpm 11.7.0。前端构建由 `apps/client/scripts/build.mjs` 执行。它使用构建锁，先清理 `apps/client/dist`，再生成模块和可部署静态资源。

构建有多个入口：

| 入口 | 工具 | 输出 |
| --- | --- | --- |
| `apps/client/src/**/*.ts`、`apps/client/src/**/*.tsx`；`index.ts` 是包入口 | TypeScript project references | `apps/client/dist/module` 中的 ESM 和类型声明 |
| `apps/client/src/dsh-ui.tsx` | esbuild，ESM splitting | `apps/client/dist/module/dsh-ui.js` 和 `dsh-[hash]` 分块 |
| `apps/client/src/boot.ts` | esbuild，浏览器 ESM splitting | `apps/client/dist/public/assets/client.js` 和 `chunk-[hash]` 分块 |
| `apps/client/src/styles/client.css` | esbuild CSS bundle | `apps/client/dist/public/assets/client.css` |

构建还复制 `apps/client/public` 和 `THIRD_PARTY_NOTICES.md`。它生成 `version.json` 和 `asset-manifest.json`。清单记录文件大小、SHA-256、发布目标和运行时配置位置。`runtime-config.js` 在部署时可修改，因此不进入资源摘要列表。

`dsh-ui.tsx` 同时属于 TypeScript 编译图和单独的 esbuild 入口。它的单独入口允许 React 岛及其依赖形成 ESM 分块。`boot.ts` 负责可部署应用。`index.ts` 负责包级 API。React 迁移按调用方完成情况收敛挂载与构建入口。

依据：根 `package.json`；`apps/client/package.json`；`apps/client/tsconfig.json`；`apps/client/scripts/build.mjs`；`apps/client/public/index.html`。

## 测试与视觉验收

TypeScript 测试使用 Node.js 内置的 `node:test`。`pnpm test:ts` 先运行 TypeScript 构建，再由 `scripts/run-ts-tests.mjs` 以单并发执行规范测试列表。页面测试按需要使用以下环境：

- 轻量 fake DOM，用于节点身份、监听器释放和精确交互测试。
- JSDOM，用于聊天、登录设置和 StrongFlow 评审等 DOM 集成测试。相关测试先用专用 `tsconfig` 编译目标。需要浏览器 bundle 的测试再用 esbuild 打包被测入口。
- 真实 Chromium，用于浏览器路由和可访问性检查。

esbuild 负责浏览器 bundle、CSS bundle 和部分测试 bundle。测试应覆盖构建后的入口、动态分块、公开静态资源和包导出。详细命令和测试分层见[测试](../testing.md)。

视觉验收必须使用真实浏览器渲染截图，并对照设计稿进行评审。截图应覆盖目标视口、主题和关键状态。评审结果应记录页面、视口、状态和差异结论。

保留的 UI-608 测试文件捕获 DOM 路径、文本、状态属性、计算样式 token 和几何信息。它们不生成像素截图，已移出规范 TypeScript 测试清单，只作参考。UI-608 指纹测试不能作为视觉验收证据。

依据：根 `package.json` 中的 `test:ts`；`scripts/run-ts-tests.mjs`；`tests/chat-page.test.mjs`、`tests/codex-login-settings.test.mjs` 和 `tests/strongflow-review-ui.test.mjs` 中的 JSDOM 使用；`tests/ui608-visual-regression.test.mjs`、`tests/ui608-page-visual-browser.test.mjs` 和 `tests/ui608-component-state-visual-browser.test.mjs`。

## DOM 契约和可访问性

迁移和重构必须保留对外可观察的 DOM 契约。以下内容需要稳定：

- `data-winwincode-client-root` 等挂载标记。
- `data-surface`、`data-capability`、`data-route-access`、`data-session-key` 和 `data-wwc-component` 等状态与身份属性。
- `role`、`aria-label`、`aria-current`、`aria-expanded`、`aria-controls`、`aria-live` 和 `aria-busy` 等可访问性语义。
- CSS 和测试使用的 `wwc-*` 类名。
- 输入焦点、文本选择、滚动位置和 keyed 列表节点身份。

这些名称同时被样式、页面逻辑、浏览器夹具和测试读取。修改它们属于契约变更。变更必须同步更新调用方、样式、测试和可访问性检查。

React 迁移应保留语义元素和原有焦点流程。状态更新不能无条件替换整棵子树。列表项必须使用领域标识作为 key。卸载必须释放订阅、定时器、事件监听器和 React root。

依据：`apps/client/src/application.ts` 中的导航和路由属性；`apps/client/src/dsh/SessionBrowser.tsx` 中的 `SessionBrowser`；`packages/browser-ui/src/status-badge.ts` 中的 `mountStatusBadge`；`tests/ui601-keyed-rendering-validation.test.mjs`；`tests/ui604-a11y-audit.test.mjs`；`tests/client-ui-components.test.mjs`。

契约字段、兼容性和生成规则见[契约](../contracts.md)。
