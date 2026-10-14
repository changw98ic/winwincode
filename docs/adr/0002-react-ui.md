# ADR-0002：前端统一使用 React

- 状态：已接受
- 日期：2026-10-14
- 实施状态：迁移待实施，现有 React 组件继续使用

## 背景

浏览器 Client 同时使用命令式 DOM 挂载与 React 组件。应用框架和页面以 `mount*` 函数组织，`dsh-ui.tsx` 挂载已有 React 界面。

## 决定

页面和组件统一使用 React 函数组件与 TSX。沿用现有 React、esbuild、TypeScript、`node:test` 和 JSDOM 工具链。

保留视图模型及其业务语义。迁移约定使用 `useSyncExternalStore` 接入发布与订阅。React 组件负责表现层。

应用使用统一 React 挂载入口。路由和应用框架随页面迁移收敛。新增依赖必须有明确需求。

## 实施边界

先迁移挂载基础、共享组件和应用框架，再逐页迁移。复杂 Chat 与 StrongFlow 页面需要同时验证完整交互和执行状态。

迁移保持页面 class、`data-*`、ARIA 属性、路由和行为稳定。测试与页面在同一改动中更新。

页面的视觉调整单独验收。迁移期间以真实渲染截图和设计稿评审确认视觉一致性。`ui608` 样式指纹检查不作为视觉验收证据。

现有 `mount*`、桥接层和 `packages/browser-ui` 只有在调用方全部完成迁移后才删除。

## 影响与验收

视图更新由 React 管理。现有视图模型仍是状态接口。页面迁移完成后，应只有统一的应用挂载路径，并通过交互回归、无障碍检查和真实浏览器验收。

ADR 的接受不代表全量 React 迁移已完成。

## 依据

- `apps/client/src/boot.ts`
- `apps/client/src/application.ts`：`mountWinWinCodeClient`
- `apps/client/src/dsh-ui.tsx`
- `apps/client/package.json`
- `apps/client/tsconfig.json`
- [前端架构](../architecture/frontend.md)
- [测试指南](../testing.md)
