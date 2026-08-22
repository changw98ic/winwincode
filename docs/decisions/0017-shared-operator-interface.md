# ADR-0017：DSH 工作台和 CLI 共用一个版本化操作接口

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.9.1`
- 格式：[`packages/contracts/src/strongflow-operator.ts`](../../packages/contracts/src/strongflow-operator.ts)
- 测试：[`tests/strongflow-operator-contract.test.mjs`](../../tests/strongflow-operator-contract.test.mjs)

## 结论

StrongFlow 对操作员只提供 `StrongFlowOperatorInvoker.invoke()` 这一个调用入口。DSH 高级工作台和 CLI 适配器提交相同的严格 JSON 请求，读取相同的严格 JSON 结果。这个接口不直接暴露 DSH 上下文、Codex 线程、模型供应商对象、原生句柄或内部存储路径。

每个请求都包含：

- `schemaVersion: 1`；
- 由调用方生成的 `requestId`，同时作为重试身份；
- 一个固定操作名；
- 该操作唯一且不接受多余字段的 `payload`。

每个结果都回传相同的 `requestId` 和操作名。客户端必须调用 `parseStrongFlowOperatorResponseForRequest()`，从而同时核对请求身份、操作名、作业 ID，以及人工审核成功结果中的四个定义身份。一个响应不能被另一个请求、另一个作业或另一个审核页面误用。

## 操作

| CLI 命令 | 操作名 | 作用 |
| --- | --- | --- |
| `create` | `job.create` | 从本地仓库和用户请求创建作业 |
| `status` | `job.status` | 读取版本化作业快照和执行锁原因 |
| `follow` | `job.follow` | 从准确游标长轮询后续事件 |
| `requirement` | `definition.requirement` | 读取当前需求正文和固定制品链接 |
| `solution` | `definition.solution` | 单独读取当前方案正文和固定制品链接 |
| `diagrams` | `definition.diagrams` | 一次读取同一定义的系统架构图和流程图 |
| `approve` | `review.approve` | 批准读取页面中的四个准确身份 |
| `reject` | `review.reject` | 拒绝读取页面中的四个准确身份 |
| `request-changes` | `review.request-changes` | 指定需求、方案或两张图重新生成 |
| `cancel` | `job.cancel` | 显式取消持久作业 |
| `resume` | `job.resume` | 从准确的中断序号恢复 |
| `artifacts` | `job.artifacts` | 按连续存储序号列出制品链接 |
| `export` | `job.export` | 导出不含凭据和私有运行时对象的 JSON 清单 |

`STRONGFLOW_CLI_COMMANDS` 是命令帮助的唯一来源。`renderStrongFlowCliHelp()` 从这份清单生成帮助，因此新增操作时不能遗漏命令说明。

## 作业、制品和事件视图

操作员作业快照单独使用 `schemaVersion: 1`，所有可空字段都明确写成 `null`，不通过字段缺失表达状态。快照包括当前流程状态、连续序号、分开的定义身份、人工审核状态、活动阶段、候选版本、中断、最后停止原因、执行锁说明和当前允许的操作。

需求、方案和两张图的读取结果同时返回正文和 `StrongFlowOperatorArtifactLink`。链接固定作业、制品种类、制品 ID、存储记录 ID、正文摘要、字节数、生产角色、阶段运行、尝试、内核事件范围、时间，以及适用的完整候选身份或候选/差异身份。解析器会核对链接、正文、候选引用种类和当前作业定义属于同一条链。

事件游标格式为 `sf-event-v1/<job>/<sequence>/<event>`。作业 ID 和事件 ID 使用 URI 编码，解析后必须重新生成完全相同的规范字符串。事件页必须：

- 只包含一个作业；
- 严格按连续递增序号排列；
- 第一项晚于请求游标；
- `nextCursor` 精确指向最后一项；
- 每页最多 500 项，单次等待最多 30 秒。

事件只暴露 StrongFlow 自己的状态、角色、阶段、候选、定义、制品链接和经过归一化的变更信息。执行中的变更事件固定为 `detailAccess: denied`，且没有冻结候选身份；执行结束的变更事件固定为 `detailAccess: available`，且必须带候选身份。这样 UI 可以实时把节点改成浅蓝色，但不能从事件接口取得具体差异；冻结后才能进入黄色节点的详细审核。

## 人工审核与认证

三个审核操作都必须提交前一次定义读取返回的四个身份：需求、方案、系统架构图和流程图。缺少任何一个身份都属于无效请求。服务实现只能在四项都等于当前定义时返回成功；成功回执还会再次核对请求与正式 `HumanReviewRecord`。旧页面或换入其他作业身份时返回 `STALE_DEFINITION`，并附上当前四个身份供客户端刷新，不能静默改用新版本。

浏览器请求使用 `local-ui` 和 `local-session` 认证，CLI 使用 `cli` 和 `local-peer` 认证。接口会校验两者配对。认证证明只出现在请求中，任何成功结果、错误、事件、制品链接或导出清单都没有认证字段。

## 错误、重试和幂等

错误信封包含稳定代码、类别、状态码、是否可重试、公开消息、可选字段位置，以及只在 `STALE_DEFINITION` 中出现的当前定义。它不包含异常对象、堆栈、请求正文、认证证明或供应商响应。

CLI 的稳定退出码为：

- `0`：成功；
- `1`：服务或内部失败；
- `2`：用法、格式、版本、游标或限制错误；
- `3`：作业或制品不存在；
- `4`：状态冲突、旧定义或重复审核；
- `5`：认证错误；
- `130`：`SIGINT`；
- `143`：`SIGTERM`。

所有请求都要求 `requestId`。服务必须把六种变更操作视为幂等：同一个 `requestId` 的重试返回第一次已经提交的结果，不重复创建作业、写人工决定、取消或恢复。读取操作也回传请求身份，便于客户端去重。

`SIGINT`、`SIGTERM`、浏览器刷新和远程连接断开只终止当前调用或 `follow` 等待，不改变持久作业。取消作业只能调用 `job.cancel`。

## 版本和兼容

这是第一个公开操作接口，属于新增接口。当前仓库尚未发布旧版本，因此没有旧格式兼容层。请求信封、响应信封、作业快照、事件、制品链接和导出清单分别带版本；未知版本在读取任何正文前失败。以后如果格式需要改变，直接迁移到一个新的正式版本并删除旧路径，不在内部维持双合同。
