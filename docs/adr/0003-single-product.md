# ADR-0003：单一产品与内部模块

- 状态：已接受
- 日期：2026-10-14
- 实施状态：模块、边界和发布配置清理待实施

## 决定

本仓库构建和发布 WinWinCode。Rust workspace crate 和 npm workspace 包是产品内部模块，不独立对外发布。

用户通过产品二进制、网页资源和完整发布包使用 WinWinCode。模块划分服务于产品实现、测试和维护。

## 实施边界

统一将第一方 crate 设为不可发布，将内部 npm 包设为私有。删除内部包独立发布流程，并收敛产品版本配置。

删除未接入产品的模块前，用产品二进制的正常依赖图核对引用关系，同时检查测试、schema、脚本和 workspace 清单。具体模块列在 [Rust 模块](../architecture/crates.md) 中。

删除独立核心发布和消费锁流程时，同步移除对应命令、测试与 schema。保留产品制品的来源、版本、签名、摘要与验收机制。

当前部分 crate 和 npm 包仍有可发布配置。此决定规定目标，相关配置尚待统一。

## 影响与验收

发布边界是完整产品。内部模块可以独立测试，但不形成独立公开版本线。

实施完成需要产品依赖图、workspace 配置和发布流水线一致。完整产品的四平台构建与制品验收继续有效。

## 依据

- `Cargo.toml`
- `package.json`
- `pnpm-workspace.yaml`
- `.github/workflows/release.yml`
- [发布指南](../operations/release.md)
