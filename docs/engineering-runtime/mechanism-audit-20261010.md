# 2026-10-10 机制行为审计

本报告供修复 Worker、Provider、Adapter 和 CodeMode 的维护者使用。审计固定基线 `e994faa55ac2baad964bea5c43a23d919497a9de`，覆盖 30 个范围。独立执行者保存实际命令、退出码、日志摘要和源码绑定。逐项事实、验收缺口及证据摘要见 [JSON 报告](mechanism-audit-20261010.json)。

共确认 6 个语义缺陷、13 个成本问题、10 个条件性问题和 1 个有效容量保护。17 个范围为 `pass`，13 个为 `red`。`pass` 表示行为或正控已复现。成本预算红测并不自动构成既有契约错误。本次提交保存 Bug 和回归；修复状态以 Beads 中的 12 个开放任务为准。

## 六个语义缺陷

| 范围 | 实际行为 | 跟踪任务 |
| --- | --- | --- |
| M12 | 同一 attempt 已合法续租，原等待者的有效输入响应仍被旧租约全等检查拒绝。 | winwincode-community.7.5.29.16.4 |
| M13 | 首个 Retry-After 已产生 DeferredUntil，却在约 19ms 内记录 retry_budget_exhausted。 | winwincode-community.7.5.29.16.6 |
| M14 | Core 合法多题输入事件无法进入 Adapter 单题投影；20 秒观察内投影为 0，未观察到 Job 失败终态。 | winwincode-community.7.5.29.16.5 |
| MP-C02 | 响应超限后，已读字节与已出现的 usage 不进入持久诊断。 | winwincode-community.7.5.29.16.12 |
| FC-R01 | CodeMode 部分 stdio 生命周期请求缺少有效截止或取消收尾。 | winwincode-community.7.5.29.16.12 |
| M19 | 旧租约失败报告先抛错，原任务随后持久化的精确终态无法进入基准收尾。 | winwincode-community.7.5.29.16.7 |

## 正式 MiMo 的租约因果

正式第三个 Provider 请求已完整成功。Provider 在 `2026-10-09T23:07:11.935Z` 完成 drain/canonical 并结束诊断，随后返回完整 2527 帧 Vec。Worker 实际收到 Vec 的时刻未落盘。随后进入本地帧摄取。冻结源码在接受每个新帧时读取、解码并核对此前全部帧，还会重建游标指纹。真实 Rust 的 2527 帧对照读取 3,191,601 条历史正文，处理 3,287,510,147 字节逻辑 JSON；正常摄取耗时 673.588 秒。这是合成 debug 回归的实测值，不是正式运行逐帧计时。

Worker 帧摄取循环没有帧数或时间预算。心跳位于同一个驱动的外层 select。原生完整正文对照中的单次 drive 持续 732,776ms，心跳出口间隔 737,226ms。正文完整、Core task_complete 和 Adapter CoreCommitted 均有持久证据。

正式 900 秒租约的 Server 续租窗口为 `[23:15:56.935Z, 23:17:56.935Z)`。窗口内没有 Worker 心跳，续租回执为 0。Device 自己的 5 秒心跳走另一条路径，不能替代 Worker 心跳。恢复后的心跳也不能复活已过期租约。第四个本地 model.open 被发送前租约守卫拒绝，实际第四个付费请求为 0。同一原 Job 和 Lease 后来持久化 failed。正式 cell 仍保留旧失败报告且缺最终记录。离线回归证明该旧报告会先抛错，遮蔽后续精确终态；本次没有重放正式 post-terminal re-entry。

本地摄取、心跳饥饿、续租窗口缺失和发送前拒绝之间有现场记录、实际行为对照及源码约束。正式运行没有每帧墙钟及每个分支的独立计时，因此不能把整段 21 分钟空档全部归给某一个函数。

## 原生对照与限制

完整正文第二次原生命令实际退出 0，持续 980.630612 秒。仅一条合成 loopback Provider 请求。2527 帧在 Provider、Adapter 和游标中逐项匹配。真实续租在释放响应前已应用到 Worker 和 Workspace。摄取跨过更新后的短租约截止，随后恢复心跳，并自然保存 Job 终态。

该对照的终态为 `CANDIDATE_PREPARATION_FAILED / WORKSPACE_UNCHANGED_CANDIDATE`。合成最终 JSON 没有通过真实工具修改工作区，因此它没有复现正式第四请求的 LeaseExpired 失败码。最大的 404,708 字节是序号 2526 的 assistant 输出帧；唯一 final 帧为序号 2527、1,235 字节。原生对照的短租约、debug 构建和输出形状均与正式现场不同。

第一轮完整正文命令实际退出 1，原因为只读观察器的数据库锁错误。此前 Job 尚在运行，清理才产生 Cancelled。它的原始非零回执仍保留。

三槽对照持续占用三个真实 OS 请求槽。第四个原请求等待时，Worker 心跳 1→4，并成功接受和应用一次续租。释放一个槽后，同一交换的 attempt 1 才发出 HTTP 请求。这证明受测等槽路径仍能服务心跳。该对照没有等待第四个 Core 或 WorkRun 完成，也没有验证跨 Device 硬链接拓扑。

断流 TLS 正控使用新 decoder 和新 attempt 发出第二个完整请求。失败的半段响应没有与新响应拼接。完整缓存复读未产生新的 HTTP 请求。

动作门禁的 trusted_now 使用外部更新的时间高水位。普通 CP 回执和新 model.open 存在实时截止守卫。过期后 delegated read-only、CoreControl 和 result-read 的权限边界尚未做真实工具回归；零工具正文对照不能证明该范围。此缺口记录在 Worker 驱动修复任务中，不新增已确认缺陷。

## 全部范围

| 范围 | 内容 | 分类 | 行为结果 |
| --- | --- | --- | --- |
| M01 | 空模型队列仍读取设备历史响应 | 成本问题 | pass |
| M02 | 逐帧投递和已完成游标复读的历史指纹处理 | 成本问题 | pass |
| FRAME_BODY_N2 | 逐帧重读并解析此前全部模型正文 | 成本问题 | pass |
| MP-C01 | SSE终态与未关闭HTTP正文的等待边界 | 条件性问题 | pass |
| MP-C02 | 响应超限时已收字节与用量诊断的保留 | 语义缺陷 | pass |
| M03 | 逐新增运行事件校验并重写全历史账本 | 成本问题 | pass |
| M04 | 运行事件与ACK更新的全量快照读写 | 成本问题 | pass |
| M05 | 精确回执查询随已发布outbox历史增长的成本 | 成本问题 | pass |
| M06 | 空待发审计查询遍历已落库历史 | 成本问题 | pass |
| M07 | Worker空待发查询遍历已发送传输历史 | 成本问题 | pass |
| M08 | 诊断 Artifact 查询先校验所有历史 Job 输出再按当前 Job 过滤 | 条件性问题 | pass |
| M09 | 非空post-action记录触发两次运行快照写入 | 条件性问题 | pass |
| M10 | 同Job多次续期后的历史回执检查成本 | 条件性问题 | pass |
| STORAGE-C01 | Worker出站队列容量边界 | 有效保护 | pass |
| STORAGE-C02 | ACK对其他流积压的读取与精确压缩 | 条件性问题 | pass |
| M12 | InputResponse 对原始租约做全等检查，拒绝已证明的同一 attempt 续期 | 语义缺陷 | red |
| M13 | DeferredUntil 被压成 None，首个长 Retry-After 被误报重试预算耗尽并硬停止 | 语义缺陷 | red |
| M14 | 多题request_user_input的Core与Adapter契约 | 语义缺陷 | red |
| RA-C01 | 交互回执、Core实际消费与崩溃重建边界 | 条件性问题 | red |
| M15 | Fusion候选上下文逐Git blob条目启动进程 | 成本问题 | red |
| M16 | CodeMode取消定时器后线程保留至原截止 | 成本问题 | red |
| M17 | CodeMode在Pending检查前深拷贝待保存值 | 成本问题 | red |
| FC-R01 | CodeMode stdio生命周期请求的截止和取消 | 语义缺陷 | red |
| M18 | 安全停派与实时进度观察范围 | 条件性问题 | red |
| M19 | 旧租约失败报告遮蔽原任务迟到终态 | 语义缺陷 | red |
| M20 | Device心跳重复加载并核对Worker历史 | 成本问题 | red |
| M21 | 未变化投影的轮询和Core审批数据库遍历 | 成本问题 | pass |
| SC-05 | 串行停止Worker对Device tick的阻塞 | 条件性问题 | red |
| M11 | 验证命令期间控制消息的服务延迟 | 条件性问题 | red |
| LEASE_CAUSALITY | 当前MiMo租约过期的现场与原生对照因果 | 条件性问题 | pass |

## 执行回归

使用仓库固定 Node 24.19.0、Rust 1.95.0 和 Corepack pnpm。Worker 原生测试还需要已构建、满足原有 64MiB 认证限制的 release Kernel helper。CodeMode 使用原生 V8，不以安装的 Codex CLI 替代。

新增的成本基线、长审计和已知红测均使用显式 `ignore`。默认门禁保留正常输入消费、owner/fence/截止保护、fresh decoder 等健康正控。Storage 复用了 canonical 夹具，由 include! 额外注册 32 个健康测试；它们不是新增的 32 个发现。存储、正文和 reader 计数只在 cfg(test) 中启用；Provider replay 计数另允许显式 test-support。原生时序和交互钩子只在 test-support 特性中启用，默认生产构建不启用该特性。显式 test-support 构建会写入本次时序日志，即使未设置 WWC_MECHANISM_TIMING；原生实测的特性和日志开销因此单独记录。Storage 的少量生产可见源码重排保持原有结果和错误传播，具体差异见 JSON 报告。

可按测试名显式执行单个审计。预算红测会保持非零，修复前应将其作为诊断输出保存：

```bash
cargo test -p winwincode-worker --features test-support --test worker_lifecycle mechanism_worker_fairness::mechanism_m11_driver_renewal_service_bound_red -- --ignored --exact --nocapture
cargo test -p winwincode-provider m13_first_retry_after_301_seconds_is_deferred_without_exhausting_four_attempts -- --ignored --nocapture
```

Core 和 CodeMode 的测试使用上游 `just test` 入口。完整 suite 的执行范围和前置资源应由当前任务决定；审计结论依赖 JSON 报告列出的实际历史回执，不把新编译成功当成行为通过。

三个 JS 范围 M18、M19、M21 可显式执行仓库调度器夹具：

```bash
AUDIT_OUTPUT="$(mktemp -d)"
mkdir -p "$AUDIT_OUTPUT/scheduler"
node tests/mechanism/scheduler-regression.mjs "$PWD" "$AUDIT_OUTPUT/scheduler"
```

`AUDIT_OUTPUT` 必须指向本次新建的私有输出目录。该命令预期在未修复的 M18/M19 上报告红测。它不属于默认 canonical 测试清单。

原生正文和三槽夹具在 `tests/mechanism/native-fullbody.mjs`、`native-slotwait.mjs`。它们需要 test-support 构建的 Server、wwc 和 Worker，`MECHANISM_BIN_DIRECTORY` 指向包含 `winwincode-server` 和 `wwc` 的新构建目录；未设置时使用仓库 `target/debug`。同目录还须有 winwincode-worker、winwincode-kernel-helper、winwincode-kernel-helper.release.json 和 winwincode-api-production.source.json。源码封条必须匹配当前 root；单次 cargo build 不会生成这些封条。Worker 和认证 helper 沿用产品 fixture 的产物变量及签名封条。已实测版本使用 macOS 进程身份收集。输出通过 `WWC_MECHANISM_AUDIT_OUTPUT` 设置，并要求全新 owned 运行目录。显式目录必须不存在，且真实父路径须位于输出根目录内。默认输出位于系统临时目录。两个入口显式清除继承的 Device Provider HTTPS 代理，合成 Provider 使用 loopback。正文夹具可能耗时十几分钟。便携包装修改了来源与输出路径，并增加启动前目录守卫和本地代理隔离；本次没有为这些包装修改再执行一轮原生长回归。

本轮 audit issue 是 `winwincode-community.7.5.29.17`。修复任务和正式 35 配置运行的状态分开记录。最新正式只读快照为 6 completed、9 failed、3 active、17 unstarted；本次审计不更改正式运行的结果。
