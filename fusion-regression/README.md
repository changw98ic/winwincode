# fusion-regression

ADR-0037 回归集：把实测翻车点和三类硬场景固化成 fixture。

## 来自真实测试

| 文件 | 场景 |
|---|---|
| `q1-defect-null-unwrap.json` | 产品链路争议 / 少数派 |
| `q1-defect-shared-map-race.json` | Fusion 曾判错 |
| `q2-root-shared-fixture-race.json` | 2:3 标准争议；禁止直接 JEV/多数否决 |
| `q2-root-float-precision.json` | 单家失手 |
| `q2-blocking-ci.json` | claimKey 规范化后仍 disputed |

## 三类硬场景

| 文件 | 要求 |
|---|---|
| `special-minority-1v4.json` | 1 对 4 不得 majority kill |
| `special-unique-truth.json` | 独见真值必须保留 |
| `special-false-unique.json` | 错误独见：先加法，验证后才可 REFUTED |

口号：**Add first, Verify later, Subtract only by proof.**

## 期望字段

- `state`: `DISCOVERED|SUPPORTED|CONFIRMED|DISPUTED|INVESTIGATING|REFUTED|ESCALATED|UNRESOLVED`
- `mustNotBe`: 禁止进入的状态（尤其 `REFUTED` / `MISSING`）
- `mustTriggerInvestigation`: 必须产生 InvestigationPlan
- `refuteAllowedOnlyWithVerifiedCounter`: 无 verified counter 不得 REFUTED
