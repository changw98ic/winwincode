# WinWinCode 社区版 UI 设计稿 · 几何硬边

本包收录当前采用的 16 张 PNG 设计稿，覆盖 15 类固定页面；独立对话包含空状态和委托后两张。图像保持原始分辨率，按页面编号命名。

这些是静态 UI 参考稿，包含示例内容；登录初始化、首次设置的后续步骤、其他标签视图以及弹窗不作为额外图稿收录。

## 页面索引

| 文件 | 页面 | 图示状态 |
| --- | --- | --- |
| [01_login.png](01_login.png) | 登录与账号初始化 | 登录态 |
| [02_onboarding.png](02_onboarding.png) | 首次设置 | 连接执行设备，第 1 步 |
| [03a_chat_empty.png](03a_chat_empty.png) | 独立对话 | 新对话空状态，架构图示意 |
| [03b_chat_delegated.png](03b_chat_delegated.png) | 独立对话 | 任务委托收至右上角 |
| [04_task_board.png](04_task_board.png) | 任务看板 | 运行中与待我处理优先，其余折叠 |
| [05_task_detail.png](05_task_detail.png) | 独立任务详情 | 单栏方案审核 |
| [07_projects.png](07_projects.png) | 项目与仓库 | 仓库列表 |
| [08_device.png](08_device.png) | 执行设备 Client | 单设备连接与可访问目录 |
| [09_plugins.png](09_plugins.png) | 插件中心 | 已安装插件 |
| [10_skills.png](10_skills.png) | 技能与指令 | 项目技能与折叠的项目指令 |
| [11_mcp.png](11_mcp.png) | MCP 与外部工具连接 | 连接列表 |
| [12_general_settings.png](12_general_settings.png) | 通用与个人设置 | 常用偏好 |
| [13_models_providers.png](13_models_providers.png) | 模型与 Provider | 新会话默认模型与 Provider 管理 |
| [14_execution_strongflow.png](14_execution_strongflow.png) | 执行与强流程设置 | 默认方式、审核节点、并发与隔离 |
| [15_diagnostics_usage.png](15_diagnostics_usage.png) | 运行诊断与用量 | 运行诊断视图 |
| [16_data_storage.png](16_data_storage.png) | 数据与存储 | 备份、恢复与导出 |

## 已采用的设计规则

- 几何硬边：暖白背景 `#F7F7EF`、近黑文字 `#171717`、黄绿色重点色 `#E8F15B`；直角或小圆角，主要操作使用少量硬阴影。
- 对话独立于任务执行。委托任务入口默认收至当前对话右上角，正文保留一行创建回执。
- 看板默认展开正在运行和待我处理；待处理筛选集中展示需要人工审核与验收的事项，未开始与已完成收成带数量的折叠行。
- 任务详情围绕当前阶段展开。方案审核展示范围与验收摘要，完整方案、流程与记录按需展开。
- 新对话展示架构图或流程图入口；本稿中的架构为示意，并非实际仓库分析结果。
- 社区版按单用户、单执行 Client 展示，支持多个仓库和并发任务；每个委托任务使用独立会话，执行隔离沿用独立工作树。
- 扩展内切换插件、技能与指令、MCP 连接；设置使用分类切换，复杂配置与历史信息默认收起。

## 阅读方式

从页面索引打开对应 PNG；文件编号与页面规划一致。`03a` 和 `03b` 是同一对话页面的两个状态。
