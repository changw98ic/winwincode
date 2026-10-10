# ADR-0041：任务状态记录与模板压缩

- 状态：已接受
- 日期：2026-10-10
- 对应任务：`winwincode-0mem`
- 关联：[ADR-0033](0033-community-engineering-runtime.md)、[ADR-0038](0038-fusion-jev-joint-bench.md)

## 决定

WinWinCode 的嵌入 Core 本地压缩使用 `Task`、`Status`、`Workspace`、
`Validation`、`Changes`、`Dependencies`、`Unverified` 七字段 YAML。
提示词只要求保留必要任务事实并去掉重复和过期过程，不向模型提供 token 目标或数字预算。新模型使用原有 Device 模型
路由。Core 手动压缩和自动压缩使用同一模板；create/resume 设置该配置，fork 继承源配置。
宿主关闭会绕过此提示词的上游 TokenBudget 实验路径。

宿主将压缩检查点的总上下文预算设置为 30,000 个 Core 估算 tokens。
系统在生成后计入完整 handoff、基础指令、恢复的仓库/角色/环境指令、工具定义和
宿主任务记录/执行绑定，再将剩余空间用于最近的用户原文。handoff 和原文尾部没有
各自独立的低限额。消息包装和裁剪标记也计入内部校验。

必需的 handoff 和恢复指令本身超限时，系统报告压缩失败，不截断 handoff，
不安装新的压缩检查点，原始历史继续保留。宿主补充文本的大小通过内部 transport
回调读取，不启动模型调用，也不写入模型请求。上游未启用宿主总预算时，仍采用
原有 20,000 tokens 用户原文策略。

计数沿用 Core 的字节估算方法，不是服务商 tokenizer 的精确计数。预算针对生成时的
检查点；后续新消息、指令或任务记录更新仍会增加上下文。

内部执行记录的 `taskHandoff` 使用同样的七字段结构，以 JSON 存入已有
`worker-codex.sqlite3` 的 `codex_run.record_json`。它是有来源的 advisory
projection，不是第二套任务状态机。Controller 的 WorkItem、验收和授权权威继续
生效，已有 Core tool receipts 保留原始事实。

记录从封存 Job 提取任务身份、目标、角色、依赖和 checkout revision。
Core patch 的成功结果提供文件操作、移动目标及 turn/call 来源。开始事件只记录未确认结果；
缺少结束事件不能解释为失败。执行结束只记录 stopped/failed/cancelled/inconclusive；
不会把执行停止变成任务验收通过。命令结束记录实际状态和退出码，计划完成不构成
验收通过。计划说明中的长篇过程和根因猜测不进入此记录。

`Changes` 记录已观察的文件操作。函数含义、新实现与既有函数的区别仍需引用
原始 patch/工具记录；程序不根据文件名猜测实现内容。普通 shell 或外部工具的
文件写入若没有 Core patch 事件，不会自动被描述为已成功应用的 patch。

每次根模型请求都附带最新宿主观察记录和封存的执行绑定。原有角色及仓库指令
保留，Contract scope/constraints/protectedScope/requiredHumanAuthority 和已分配
criteria 单独恢复。只存在于聊天里的用户范围变更、临时边界和决定仍需进入
handoff 的 Task，不能假定 AGENTS.md 会恢复它们。模型必须核对比宿主快照更新的
Core 结果。独立 Fusion 成员的盲评输入继续由其已有闭包管理。

## 持久化与重放

`model_call_task_context` 在同一私有 SQLite 中按 run key 与 Core request ID
冻结宿主补充文本，同时记录现有模型输入身份摘要和补充文本的 SHA-256。传输元数据更新不改变该身份。重放使用首次冻结的
文本，新请求才使用最新记录。改变模型输入或损坏冻结文本会被拒绝。该表只保存
任务状态与执行绑定，不复制完整聊天或工具输出。

表创建是 additive、幂等操作。已有 run 没有 `taskHandoff` 时，从其封存 Job
初始化；不会把旧历史摘要解释成新验收事实。不会修改已有产品表、回执或数据。
开发期版本切换遵循 ADR-0033；不承诺旧二进制读取新增的内部 JSON 字段。

结构化视图限制为 128 个改动文件、16 个近期检查、64 个待确认条目（包含超限定位标记）。
超出视图的记录明确指出回到 Core receipts 定位。原始事实不随视图裁剪删除。
每个来源保留实际 turn/call 标识；原始命令输出不会进入 taskHandoff。

## 验收

验收覆盖七字段记录、缺失工具结果、已应用文件操作、计划完成与验收的区别、
数据库 reopen、同一请求的冻结重放、原请求冲突、错误工作树绑定，以及 30,000 tokens 总预算下
完整保留长 handoff、保留最近用户原文、计入宿主上下文，以及超限时
拒绝检查点并保留原历史。格式、编译、lint 和相关运行回归必须通过后，
才能关闭任务。

258k 实验支持模板形态的可行性。它没有证明实际运行的恢复质量或性能改善。
本改动不把那次实验的 token 数、耗时或输出正确性作为产品性能保证。
