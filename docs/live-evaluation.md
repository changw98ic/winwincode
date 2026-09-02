# 真实模型与真实仓库评估

这个入口用于在确定性测试已经通过后，测量一份固定 `DeliverySpec` 在真实 DSH 模型路由、内嵌 Codex Core 和隔离 Git 仓库上的完整交付结果。它可能消耗提供商额度，默认不会启动。

## 运行前准备

1. 构建当前 TypeScript 包和本机原生内核：

   ```bash
   corepack pnpm build:ts
   corepack pnpm build:native
   ```

2. 选择一个本地 Git 仓库和一个完整的 40 或 64 位小写 commit ID。运行器会从这个固定提交建立独立副本，不会修改源仓库。
3. 准备配置文件。配置中只写凭据环境变量名称，不写 API Key。
4. 从提供商当日价格表填写价格和来源。`UsdMicros` 表示一美元的百万分之一；例如每百万 token 1 美元应填写 `1000000`。

## 配置形状

下面是完整形状。路径、commit、模型、价格、目标和验收条件都必须替换成当前评估的真实固定值；`deliverySpec.repository.locator` 必须与 `repository.sourcePath` 指向同一路径，`baseRevision` 必须与 `expectedCommit` 完全相同。

```json
{
  "schemaVersion": 1,
  "runId": "RUN_ID",
  "projectionVersion": 2,
  "repository": {
    "sourcePath": "/absolute/path/to/repository",
    "expectedCommit": "0123456789abcdef0123456789abcdef01234567"
  },
  "provider": {
    "route": "deepseek",
    "model": "deepseek-v4-flash",
    "apiKeyEnv": "DEEPSEEK_API_KEY",
    "baseURL": null,
    "reasoningEffort": null,
    "timeoutMillis": 120000
  },
  "budgets": {
    "maxWallTimeMillis": 3600000,
    "maxModelCalls": 40,
    "maxTurns": 8,
    "maxTokensPerCall": 8192,
    "maxTotalTokens": 200000,
    "maxCostUsdMicros": 5000000,
    "pricing": {
      "source": "provider price sheet URL and retrieval date",
      "inputUsdMicrosPerMillionTokens": 1000000,
      "outputUsdMicrosPerMillionTokens": 1000000,
      "cacheReadUsdMicrosPerMillionTokens": 1000000,
      "cacheWriteUsdMicrosPerMillionTokens": 1000000
    }
  },
  "deliverySpec": {
    "schemaVersion": 3,
    "id": "SPEC_ID",
    "deliveryId": "DELIVERY_ID",
    "revision": 2,
    "title": "交付标题",
    "goal": "要完成的具体结果",
    "scope": ["允许修改的范围"],
    "outOfScope": ["明确排除的范围"],
    "constraints": ["必须遵守的约束"],
    "acceptanceCriteria": [
      {
        "schemaVersion": 3,
        "id": "CRITERION_ID",
        "description": "可验证的完成条件",
        "verificationMethod": "运行的检查或人工判断方法",
        "required": true
      }
    ],
    "sourceRef": null,
    "publicationTarget": null,
    "repository": {
      "schemaVersion": 3,
      "kind": "local-git",
      "locator": "/absolute/path/to/repository"
    },
    "baseRevision": "0123456789abcdef0123456789abcdef01234567",
    "maxReworkAttempts": 1,
    "createdAtMillis": 1
  },
  "solution": {
    "id": "SOLUTION_ID",
    "summary": "已经由人确认的实施摘要",
    "approach": ["实施步骤"],
    "components": [
      {
        "id": "COMPONENT_ID",
        "label": "组件名称",
        "responsibility": "组件职责",
        "kind": "component",
        "trustBoundary": "边界说明",
        "unresolved": false,
        "repositoryPathPrefixes": ["src"]
      }
    ],
    "connections": [
      {
        "id": "CONNECTION_ID",
        "from": "platform:codex-core",
        "to": "COMPONENT_ID",
        "label": "执行已批准的修改"
      }
    ]
  },
  "humanDecisions": {
    "planReview": {
      "action": "approve",
      "comments": "批准这份固定方案",
      "requestedChanges": []
    },
    "deliveryReview": {
      "action": "approve",
      "resolution": "批准当前候选、证据和通过结论"
    }
  },
  "execution": {
    "commitMessage": "Freeze evaluated candidate"
  }
}
```

`provider.route` 和 `provider.model` 必须使用当前 DSH `llm-pi-ai` 已安装目录中的原始身份，例如 `deepseek` 和 `deepseek-v4-flash`。运行器不会重新声明协议、上下文窗口、推理格式或工具消息规则；这些能力全部从 DSH 目录解析，并在发起模型请求前检查文本输入、上下文和所选推理强度。不要把同一个官方提供商改成某个运行专用别名，否则 DSH 会把它当成另一条手工声明的路由，无法继承原目录的模型兼容信息。

`baseURL` 为 `null` 时使用 DSH 目录中的端点；只有代理或本地测试才填写一个覆盖 URL。覆盖 URL 不能带用户名、密码、查询参数或 fragment。`apiKeyEnv` 的名称必须含有 `KEY`、`SECRET` 或 `TOKEN`，使 Codex 默认 Shell 环境规则排除该变量；测试会实际检查 Executor、Reviewer 和 Verifier 的命令环境中没有提供商凭据。第一版有六个正常模型轮次，并为 Reviewer、Verifier 各保留一次结果纠正机会，因此 `maxTurns` 不能小于 8，`maxModelCalls` 不能小于 `maxTurns`。格式或证据协议错误的结果仍保留在追加式 RuntimeSessionLedger 中；只有最新纠正轮次的完整结果可以进入 Evidence 和 Verdict。证据清单把可引用的 `citation` 与只读的 `outcome` 分开；模型只能把 `citation` 中的 `type` 和 `event_id` 原样放入结果，不能复制 `outcome`，也不能根据命令内容重新分类。纠正轮次会列出被拒绝的组合和同一事件允许使用的组合，避免让模型猜测错误发生在哪里。两次尝试都不符合验收协议时，运行直接失败，不生成交付结论。

## 启动

先在环境中设置配置引用的凭据，再显式加入真实评估：

```bash
export PROVIDER_API_KEY='...'
corepack pnpm evaluate:live \
  --live \
  --config /absolute/path/to/live-evaluation.json \
  --output /absolute/path/to/evaluation-results
```

命令会先运行：

```bash
node --test tests/delivery-full-keyless.test.mjs
```

这个前置检查没有跳过参数。检查通过后才会连接模型提供商。

## 查看结果

运行中的结果持续写入：

```text
OUTPUT/RUN_ID/result.json
```

`state` 的最终值为 `completed`、`failed`、`interrupted` 或 `budget-exceeded`。命令成功时会打印状态和结果路径；失败或收到 `SIGINT`/`SIGTERM` 时只打印稳定错误代码和已有结果路径。相同 `runId` 不会覆盖之前的目录。

结果会保存规范化后的完整 `DeliverySpec`、已批准方案、人工决定脚本和执行输入，并为每一项保存 SHA-256。Executor 只修改隔离工作区中的源码。执行轮结束后，阶段控制器把这些精确改动冻结成 Git candidate，再从该 commit 建立一份不含 Executor 忽略文件和生成物的干净审核副本；Reviewer 和 Verifier 随后在这份只读副本中审核同一个 commit、tree 和 Diff。原始 Session 日志继续由 DSH/Codex 保存，评估结果只带能追溯到原始事件的安全投影。

`measures` 字段从上述事实派生五组结果：

| 组别 | 直接回答的问题 |
| --- | --- |
| `completeness` | 必需和可选验收条件分别通过、失败、无法判断、基础设施错误或缺失多少 |
| `confidence` | 当前候选的直接证据、评审发现和必需验证角色是否完整 |
| `stability` | 阶段失败、返工、运行错误、恢复、中断和基础设施问题发生了多少 |
| `humanDependence` | 人工阶段、Attention、执行审批和用户输入有多少，是否仍在阻塞 |
| `efficiency` | 实际时间、阶段耗时、模型调用、token、费用和观察到的 Agent 并行数 |

每个数字、状态和真假判断都带 `sourceRefs`。结果没有总分；证据充分的失败会保持“交付失败、结论可信”，不会被压成一个容易误读的数字。`falseSuccessRisk` 和 `falseFailureRisk` 会分别显示“已声称成功但证明不足”和“证明完整但运行仍报失败”的矛盾。

## 重新计算测量结果

保存的 `result.json` 可以离线重新计算：

```bash
corepack pnpm measure:evaluation \
  --result /absolute/path/to/evaluation-results/RUN_ID/result.json \
  --check
```

`--check` 会把重新计算的结果与文件中保存的 `measures` 逐字段比较；不一致时以非零状态结束。不加 `--check` 时，命令把重算结果输出到标准输出。需要独立文件时可加：

```bash
corepack pnpm measure:evaluation \
  --result /absolute/path/to/evaluation-results/RUN_ID/result.json \
  --output /absolute/path/to/measures.json
```

确定性场景与真实运行使用同一个计算实现，但分别标记为 `deterministic` 和 `live`。报表应分开显示，不应把脚本化时间、token 或成功率与真实提供商结果求平均。完整计算口径见 [ADR-0026](decisions/0026-explainable-delivery-measures.md)。
