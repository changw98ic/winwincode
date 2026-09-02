# ADR-0029：Client 采用单一快照、浏览器草稿与 keyed DOM 模块

- 状态：已接受
- 日期：2026-09-02
- 对应任务：`winwincode-0m0`（UI-101）
- 上层运行边界：[ADR-0028](0028-control-plane-worker-migration.md)

## 背景

`apps/client/src` 当前有 24 个顶层 TypeScript 文件。`application.ts` 同时处理 Hash
路由、认证与 Scope 选择、路由查询、动态加载和页面挂载；页面和 view-model 也都在同一层。
Chat、StrongFlow、Settings、Local Operations、Local Decisions 和 Enterprise 已有各自的
view-model，但多个列表在每次状态通知时使用 `replaceChildren()` 重新创建行。导航已经声明
Settings 和 Approvals，应用壳目前只挂载 Chat、StrongFlow 和 Enterprise。

现有正确边界继续保留：`control-plane-client.ts` 是浏览器访问 Server 的唯一网络门面；
feature view-model 从 HTTP 查询和 WebSocket 事件构造只读视图，页面不直连 Worker，也不创建
另一套 Delivery、ProductSession、Approval 或执行状态。

本决定把目录、状态所有权和 DOM 更新方式固定下来，但不要求一次性重写全部 Client。

## 决定

### 1. 目录与依赖方向

目标结构只有一条当前路径：

```text
apps/client/src/
├── boot.ts                         # 浏览器启动
├── application.ts                  # 只做组合：创建 core，注册并挂载 feature
├── generated/                      # canonical schema 生成物，不手改
├── core/
│   ├── control-plane-client.ts     # HTTP/WebSocket 唯一网络门面
│   ├── runtime-config.ts           # 部署时 Server URL
│   ├── router.ts                   # ClientRoute 的解析、格式化与导航
│   ├── auth-context.ts             # 当前 AuthSession 的只读上下文
│   ├── scope-context.ts            # 从 authorizedScopes 解析当前 Scope
│   ├── presentation-state.ts       # transport/interaction 状态到可见信号的纯映射
│   └── rendering/
│       ├── mounted-view.ts         # update/close 生命周期
│       └── keyed-collection.ts     # 保留节点身份的列表更新
├── components/                     # 无业务知识的 DOM modules
│   ├── app-shell.ts
│   ├── action-button.ts
│   ├── empty-state.ts
│   ├── form-field.ts
│   └── status-notice.ts
├── styles/
│   ├── tokens.css                  # 唯一视觉常量
│   ├── base.css                    # reset、字体、焦点和响应式基础
│   ├── components.css              # 通用 components
│   └── features/                   # 与 feature 同名的页面样式
└── features/
    ├── auth/
    ├── chat/
    ├── strongflow/
    ├── settings/
    ├── approvals/
    ├── local-operations/
    └── enterprise/
```

依赖方向固定为：

```text
boot → application → features → core + components + generated
                            components → core/rendering
                                  core → generated
                             generated → 无 Client 内部依赖
```

- `core` 只拥有浏览器运行机制：网络门面、路由、认证上下文、Scope 上下文、生命周期和
  DOM 更新算法；它不导入任何 feature，也不保存业务对象。
- `components` 是深 module：以少量不可变 props 和回调隐藏 DOM、ARIA、焦点及样式类；
  它不导入 generated 合同、Control Plane Client 或 feature view-model。
- `features/<name>` 拥有该页面的路由适配、view-model、页面组合和 feature 专用展示；
  feature 之间不互相导入。跨 feature 行为提升到 `core` 或无业务知识的 `components`。
- 根 `application.ts` 是唯一组合点，可以导入全部 feature；它不发业务 query/command。

### 2. 路由、认证和 Scope seam

`core/router.ts` 的 interface 是一个可辨识联合 `ClientRoute`，而不是散落的
`URLSearchParams`。URL 保存可分享的导航选择：surface、完整 Scope 身份和当前实体 ID；
表单值、筛选输入、展开状态和错误信息不进入 URL。

`auth-context.ts` 只暴露 Server 返回的 `actor`、`authorizedScopes`、到期时间和恢复状态。
`scope-context.ts` 用路由中的 organization/workspace/project/repository ID 与
`authorizedScopes` 逐字段匹配。只有一个兼容 Scope 时可以自动选择；有多个候选而路由没有
明确身份时显示 Scope 选择器，不能再取数组第一项。路由引用未获授权的 Scope 时显示访问
错误，不能静默切换到另一个 Scope。

feature 的最小启动输入为：

```text
ControlPlaneClient + Actor + exact Scope + typed route IDs
+ nextRequestId + subscriptionId
```

认证或 Scope 改变时，应用壳先 abort 查询、关闭订阅和旧 feature，再创建新 feature。
feature 不能读取全局认证变量，也不能自行扩大 Scope。

### 3. Server snapshot、实时事件与浏览器草稿的所有权

| 数据 | 唯一所有者 | Client 可以做什么 |
| --- | --- | --- |
| AuthSession、Actor、authorizedScopes | Control Plane | 缓存当前响应并展示恢复/失效状态 |
| ProductSession、消息、Delivery、StageRun、Approval、Attention、Settings、Worker、企业资源及其 revision/status | Control Plane | 校验并冻结 HTTP snapshot 或 WebSocket 投影，按 ID 派生只读显示 |
| query 游标、subscription 游标、请求中/重连/分页/错误 | 对应 feature view-model | 控制读取生命周期；不改写业务状态 |
| 当前 surface、Scope 和实体选择 | typed route | 解析、验证并传给 feature |
| 未提交的输入、选择、过滤、排序、展开、滚动和焦点 | 挂载中的页面 module | 仅存浏览器内存，以实体 ID 作为草稿 key |
| 已提交但 Server 尚未确认的表单 | 页面 module | 保留原草稿并显示 busy；失败时保留，成功并刷新 snapshot 后清除 |
| secret/`vaultLocator` 输入 | 当前表单控件 | 只写；成功、取消或 unmount 立即清除，不放入 URL、缓存或持久存储 |

WebSocket 是 Server 事实的实时入口，不是第二个浏览器业务模型。view-model 只接受合同校验、
Scope/实体身份、revision 和 cursor 均匹配的事件；出现 gap、reset 或 revision 冲突时丢弃受影响
缓存并重新 query snapshot。页面不能通过“看起来应该进入下一状态”来推进业务对象，也不做
乐观 Delivery/Session/Approval 状态迁移。command 完成后以响应或后续 snapshot 为准。

浏览器草稿与 Server snapshot 分开保存。snapshot 更新时，仍对应同一实体的未提交草稿保留；
实体消失、Scope 改变、提交成功或页面关闭时删除草稿。draft 不复制 revision/status，也不成为
可恢复的业务记录。

### 4. keyed rendering

继续使用浏览器 DOM，不引入第二个 UI runtime。每个页面只在 mount 时创建静态骨架；之后通过
`MountedView.update(props)` 更新属性、文本和现有节点。只有路由切换或 `close()` 可以清空
feature root。

`keyedCollection` 的 interface 固定为：父节点、只读 items、`key(item)`、`create(item)`、
`update(node, item)` 和可选 `remove(node)`。实现维护 `Map<Key, Node>` 并执行：

1. 同一次更新出现重复 key，测试和开发构建立即报错；
2. 已存在 key 复用原节点并只调用 `update`；
3. 新 key 创建一次节点，并按输入顺序移动/插入；
4. 消失的 key 先执行清理，再移除节点；
5. `close()` 清理全部 listener 和节点引用。

Server 实体使用 canonical ID 作 key；关联行使用含全部稳定 ID 的组合 key。禁止使用数组下标、
标题、可翻译文字或当前状态作 key。输入控件的 DOM 节点、value、selection 和 focus 在 key 未变
时必须保留；只有用户切换实体、显式 reset 或提交成功才能用 snapshot 初始值重置。

状态通知不会自动触发整页 DOM 重建。更新前后 snapshot 与展示 props 都未变化时不写 DOM；
大列表先 keyed 更新，分页和虚拟化只在测量证明需要时加入。keyed collection 的 interface 同时
是测试 seam：测试要证明节点 identity、焦点、草稿、顺序、删除清理和重复 key 失败。

### 5. 设计令牌与状态语义

令牌是 `apps/client` 的唯一视觉常量。feature CSS 只能引用 `--wwc-*` custom properties，
不能新增散落的十六进制颜色、任意间距或层级。首批令牌如下：

| 类别 | 令牌 |
| --- | --- |
| 画布与文字 | `--wwc-color-canvas: #f5f7fb`、`--wwc-color-surface: #fff`、`--wwc-color-surface-subtle: #f8fafc`、`--wwc-color-text: #172033`、`--wwc-color-text-muted: #64748b`、`--wwc-color-border: #dbe2ea` |
| 操作与焦点 | `--wwc-color-action: #2563eb`、`--wwc-color-action-strong: #1d4ed8`、`--wwc-color-focus: #1d4ed8`、`--wwc-focus-width: 3px`、`--wwc-focus-offset: 2px` |
| info | `--wwc-color-info-text: #1e3a8a`、`--wwc-color-info-surface: #eff6ff`、`--wwc-color-info-border: #2563eb` |
| success | `--wwc-color-success-text: #166534`、`--wwc-color-success-surface: #f0fdf4`、`--wwc-color-success-border: #16a34a` |
| warning | `--wwc-color-warning-text: #92400e`、`--wwc-color-warning-surface: #fffbeb`、`--wwc-color-warning-border: #d97706` |
| danger | `--wwc-color-danger-text: #991b1b`、`--wwc-color-danger-surface: #fef2f2`、`--wwc-color-danger-border: #dc2626` |
| 间距 | `--wwc-space-1/2/3/4/6/8/12` = `0.25/0.5/0.75/1/1.5/2/3rem` |
| 字体 | `--wwc-font-family` = `Inter, ui-sans-serif, system-ui, sans-serif`；`--wwc-font-size-xs/sm/md/lg/xl` = `0.75/0.875/1/1.25/1.5rem`；`--wwc-font-weight-regular/medium/strong` = `400/600/700` |
| 形状 | `--wwc-radius-sm/md/lg` = `0.375/0.5/0.75rem`；`--wwc-border-width: 1px` |
| 层级 | `--wwc-layer-base/sticky/popover/dialog/toast` = `0/10/20/30/40` |
| 响应式 | compact `48rem`；workspace-stack `64rem` |

CSS media query 直接使用这两个固定 breakpoint 值，因为 custom property 不能可靠用于媒体条件。
`≤48rem` 时导航、side panel 和表单操作单列；`≤64rem` 时 StrongFlow/Enterprise 工作区和图表
改单列。所有宽度下正文和操作都不能要求水平滚动。

状态颜色只是辅助信号。每个状态都必须同时有可见文字和非颜色信号：info 用 information 图标，
success 用 check 图标，warning 用 warning 图标，danger 用 error 图标；图标 `aria-hidden`，完整
含义留在文字中。当前项使用 `aria-current`，忙碌区域使用 `aria-busy`，字段错误使用
`aria-invalid` 与 `aria-describedby`。新错误用 `role="alert"`，普通进度用 `role="status"`。

业务状态到 `info/success/warning/danger/neutral` 的转换是无副作用的展示映射，不保存、推进或
重命名 Server 状态。比如 reconnecting/revision-conflict 是 warning，确认成功是 success，
permission denied 和 command error 是 danger；页面仍显示原本的明确状态文字。

### 6. 最小 module interface

所有 DOM module 统一返回：

```ts
interface MountedView<Props> {
  readonly root: HTMLElement
  update(props: Readonly<Props>): void
  close(): void
}
```

最小 components 清单及输入/输出如下：

| Module | 输入 | 输出/事件 |
| --- | --- | --- |
| `AppShell` | surfaces、active route、认证摘要、Scope 摘要 | `surfaceRoot`、原生导航事件、`update/close` |
| `ActionButton` | label、variant、busy、disabled、accessible name | `onActivate`；实现负责 busy/disabled/键盘语义 |
| `StatusNotice` | tone、icon、title、detail、live mode、actions | `MountedView`；实现负责 role、live region 和非颜色信号 |
| `FormField` | id、label、help、error、required 和一个原生 control | `MountedView`；实现连接 label/help/error 的 ARIA 引用 |
| `EmptyState` | title、detail、可选主操作 | `MountedView`，主操作回调 |
| `KeyedCollection<T, Key>` | items、稳定 key、create/update/remove | `update(items)/close`；保证节点 identity 与清理 |

业务操作仍是 feature view-model 的 interface。component 只发出用户意图，不接收 Client、
requestId、revision 或 command 名称。页面把回调翻译为 view-model 调用，view-model 再通过
唯一 Control Plane Client seam 执行 query/command/subscribe。

## 迁移顺序

每一步都可独立通过类型检查和页面测试；不保留旧路径 re-export、别名、双读或双写。

1. 在现有页面不搬家的前提下先加入 tokens、`MountedView`、`KeyedCollection` 和五个基础
   components，并为节点 identity、焦点和状态信号写测试。
2. 建立 typed router、AuthContext、ScopeContext 和 AppShell；应用壳停止直接发业务查询，
   同时把现有 Settings、Approvals/Local Decisions 与 Local Operations 挂到明确 route。
3. 以完整 feature 为单位迁移文件和所有 import/test/build entry：Auth → Settings/Local
   Operations → Chat → StrongFlow → Enterprise。每次移动立即删除原顶层文件，不留转发文件。
4. 在每个 feature 内先保持现有 view-model interface，再把 page 的列表替换为 keyed update；
   优先迁移有输入草稿和长列表的 StrongFlow、Chat、Approvals 和 Enterprise。
5. 所有 feature 完成后，收紧源码检查：根目录只允许 boot/application，禁止页面直接网络、
   禁止 update 阶段清空 feature root，禁止 feature CSS 使用令牌外视觉常量。

## 验收与影响

每个迁移步骤至少运行：

```bash
corepack pnpm --filter @winwincode/client typecheck
corepack pnpm --filter @winwincode/client build
node --test <本次改动对应的 Client tests>
```

浏览器验收必须覆盖：键盘导航、焦点环、状态文字和图标、草稿在实时刷新后的保留、Scope
切换清理、WebSocket reset 后重查 snapshot，以及 48rem/64rem 两个布局转换点。

本决定增加少量通用 DOM module，但避免引入框架、全局业务 store 和第二套状态机。代价是各
feature 仍需维护明确的 snapshot 聚合；收益是网络、所有权、草稿和渲染 identity 都有唯一
位置，后续页面可逐个迁移而不需要一次性重写 Client。
