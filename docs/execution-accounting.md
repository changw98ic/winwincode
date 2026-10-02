# 执行账务追补

执行结果与账务完整性分别记录。已经成功完成并通过原有产物、身份和验收校验的任务，不因缺少 Provider usage 改为执行失败。Worker 优先恢复持久化账本的已知下界；总用量尚不完整时发送 `accountingStatus=unknown`、`tokens=null`，保留 `knownTokens`，缺少费用时保持 `costMicrounits=null`。Delegated 模式已通过验收的候选同样可以冻结，冻结后补记不修改原始计数；账务缺失时，新增付费调用仍受原有有限预算约束。未知账务不填零总量，也不代表账务结清；Server 确认成功终态、释放执行资源并保留账务待补记，重启重放沿用原始终态。

账务追补使用独立入口 `/internal/v1/execution-port/accounting`，只更新费用事实和账务投影。执行终态、取消状态、租约和原始 unknown 标记保持可审计。

Server 必须显式配置 `WWC_SERVER_ACTION_SIGNING_KEY_HEX`，Worker 必须配置对应的 `WWC_WORKER_ACTION_SIGNING_KEY_HEX`。该密钥由受信任的 Device 适配器持有；账务 MAC 与动作许可 MAC 使用不同域。这里验证的是受信任适配器导出的 Provider 回执，而不是 Provider 独立签发的密码学账单。缺少或错误的密钥拒绝追补。

Worker 的独立后台任务查询 Server 已完成或被替换的 attempt，再读取本机 Provider 存储。它不调用 Provider，也不经过执行凭证续期入口。GET 使用独立账务令牌，返回最多 100 个租约及 `nextOffset`；后续页通过 `X-Accounting-Offset` 请求，结束后从零开始重新扫描。POST 使用同一令牌并提交签名的账务声明，正文上限 2 MiB。

声明包含原始 lease 身份、已关闭 attempt 的完整调用清单，以及已经收到的 Provider 回执。每份回执绑定 slot、model exchange、Provider、回执身份、源摘要和可选 token/费用。Server 将 lease 与持久化的原始 attempt 对齐，拒绝外来租约、修改后的清单、跨任务重复使用的回执，以及修改已知数值的重放。

Device 在冻结调用清单后禁止该 attempt 新建模型调用。取消后的真实响应仍保存到独立账务字段；运行时的取消 fence 不会删除这些收费证据。未完成的调用、缺少用量的失败调用，以及 JEv 失败尝试都保留未知 slot。没有源回执的 attempt 不生成零费用声明。

token 与费用可以分批补记。Server 按 attempt 去重、相加，所有相关调用的某项指标完整后才给出该项 job 总量。完整费用不存在时继续保留 unknown。若 Provider 后续提供独立账单，受信任对账适配器可提交同一清单和回执身份下的缺失费用及相应源摘要；普通模型输出或用户提交的总额不能直接进入存储消费者。

源事实先持久化，账务投影随后更新。两步之间中断时，重放同一声明会修复投影，不会重复计费。短生命周期 Worker 在终态交换后会唤醒后台对账，并给予两秒的有界交接时间；未完成的追补由以后连接的 Worker 从 Device 持久化源继续处理。没有活动 Worker 时，可由持有该账务权威的受信任适配器调用独立入口。

密钥轮换时协调 Server 与 Worker。Device 保存的是源回执，重新连接的 Worker 使用当前密钥签名；旧签名和旧账务令牌失效。此通道不能重新执行任务或授予新的动作权限。

Worker 在现有 SQLite 库中增加独立的 `execution_terminal_outcome` 表，与 run 状态和待发消息在同一事务中保存首次终态。ACK 可以清理待发队列，原始终态快照继续用于恢复；账务补记不修改快照。升级时，仍保留在旧 outbox 中的终态会先转存；旧版本已删除且未留存完整正文的历史消息无法凭空恢复，不能据此声明历史重放已经修复。旧程序可忽略新增表，回退后再次启动新程序仍能读取快照。
