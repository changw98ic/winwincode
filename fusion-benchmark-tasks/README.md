# fusion-benchmark-tasks

ADR-0038 Phase 2 任务源：`agent-benchmark-tasks`（20 道独立多语言开发题）。

## 来源

- GitHub: https://github.com/changw9813/agent-benchmark-tasks
- 状态：公开题库 A，**已归档只读**（2026-09-22）
- 本地：`fusion-benchmark-tasks/agent-benchmark-tasks/`
- 协议：`PROTOCOL.md` / `VALIDATION.md` / `catalog.json`
- 许可：MIT（题目与自有代码）

## 任务清单（20）

| ID | 任务 | 环境 |
|---|---|---|
| rust-001 | JSON 结构差异引擎 | Rust 1.89.0 |
| rust-002 | SRT 字幕时间偏移与冲突检测 | Rust 1.89.0 |
| rust-003 | 联系人 CSV 校验器 | Rust 1.89.0 |
| go-001 | 有界并发重试调度模拟器 | Go 1.24.7 |
| go-002 | 多资源预约状态机 | Go 1.24.7 |
| go-003 | Webhook 去重与版本归并 | Go 1.24.7 |
| cpp-001 | TLV 二进制解析器 | C++20 / GCC 14.2 |
| cpp-002 | 可调整容量 LRU | C++20 |
| cpp-003 | 区间裁剪与归并 | C++20 |
| dotnet-001 | 订单优惠金额计算器 | .NET 8 |
| dotnet-002 | 库存预占与释放状态机 | .NET 8 |
| dotnet-003 | 表单字段规则校验 | .NET 8 |
| python-001 | JSONL 日志筛选与错误汇总 | Python 3.12 |
| python-002 | 批量重命名计划与冲突检查 | Python 3.12 |
| python-003 | 虚拟目录树变更引擎 | Python 3.12 |
| node-001 | 看板任务排序状态机 | Node 24 |
| node-002 | 跨日工时账本汇总 | Node 24 |
| bun-001 | HTTP 模拟服务路由引擎 | Bun 1.2 |
| zig-001 | 有界 RLE 字节编解码器 | Zig 0.14 |
| zig-002 | 受限根路径规范化器 | Zig 0.14 |

单题和全批次都不设 token、调用、墙钟或费用上限；同一工具请求重复超过五次时
立即停止并计为未通过。
评分 = 隐藏测试通过权重 / 总权重 × 100。

## 与 ADR-0038 四组基线对齐

每题跑 A/B/C/D，且每组各产生五个对照结果一次：

| Arm | Fusion | JEV |
|---|---|---|
| A | OFF | OFF |
| B | OFF | ON |
| C | ON | OFF |
| D | ON | ON |

| 对照 | 组成 |
|---|---|
| `glm5.1flash` | 单模型 |
| `mimov2.6pro` | 单模型 |
| `ds4.1flash` | 单模型 |
| `qwen3.8flash` | 单模型 |
| `fusion(4)` | 独立调用四模型各一次，再聚合一次 |

所有模型调用统一使用 `max` 思考强度。四个单模型对照各执行一个独立 run；
`fusion(4)` 是额外的一次独立四模型聚合 run，四个成员各调用一次后聚合一次，
不复用单模型对照输出，也不递归聚合。主矩阵包含 320 个单模型 run 和 80 个
Fusion run，共 400 个评测 run；JEV 的 Context-only / Judge-only / Full 消融
另有 240 + 60 = 300 个 run。

任务分类（跑完自动打标）：Consensus Correct / Complementary Truth / Minority Truth / False Consensus / False Minority / Evidence Conflict。

**禁止**人工规定「谁必须错」。

## 运行注意

- 宿主：Python 3.9+、Docker；镜像按 digest 固定；Linux arm64 实测。
- `python3 tools/bench.py list | build-env | init | smoke`
- `smoke` 只是公开样例，**不代表完整成绩**；裁判用私有隐藏测试重跑冻结源码。
- **提交必须 push 到公开提交仓**：<https://github.com/changw98ic/agent-benchmark-submissions>
  - 分支/标签：`submit/<task-id>/<run-id>`；路径：`<task-id>/...`
  - 裁判只从该仓拉冻结源码；不 push = 没交卷
  - **禁止**污染公开题库 A、禁止触碰私有裁判库 B
  - 不把维护者 gh 配置 / 钥匙串 / 裁判库 B 的代码或 token 提供给被测 agent
- 本地开发可在临时工作区；**最终成绩以 push 后的提交内容为准**。
- 旧评测记录、旧运行历史和旧 Snapshot 事实不并入新结果，也不迁移。

## 本地目录

```text
fusion-benchmark-tasks/
├── README.md                      ← 本说明
├── agent-benchmark-tasks/         ← 克隆的公开题库 A（只读参考）
└── agent-benchmark-submissions/   ← 本地镜像；远端 https://github.com/changw98ic/agent-benchmark-submissions
```

## Phase 2 用法（ADR-0038）

1. 锁定任务 commit + 环境 image ID
2. 对每题跑 A/B/C/D × 四个单模型与独立 `fusion(4)`；另跑三组 JEV 消融
3. 记录 `benchmark_run / task_run / agent_run / claim / evidence / jev_rebuild / context_snapshot`
4. **push 提交物**到 https://github.com/changw98ic/agent-benchmark-submissions（`submit/<task-id>/<run-id>`）
5. 输出 Quality / Context / Efficiency / Fusion / JEV 面板指标

卡死判定：工具请求身份由工具名、目标资源、规范化参数和请求内容摘要组成，
排除请求 ID、时间戳和进度元数据。同一身份累计出现 6 次，即重复请求相同内容
超过 5 次；第 6 次必须在执行前拦截，记录 `STUCK_TOOL_REPEAT_LIMIT` 并结束
本机 runner。该 run 不得计为通过，也不得移出固定分母。

正式评测 Runner 固定为本机 `macOS 26.5.1 aarch64-apple-darwin`，不需要另行
选择或确认。
