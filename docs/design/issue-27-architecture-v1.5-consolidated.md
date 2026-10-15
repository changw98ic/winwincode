# WinWinCode 架构设计 v1.5（Issue #27 整理合并稿）

- 版本：**草案 v1.5 · 整理合并稿**，不是已批准的 v1.6。
- 状态：待评审、待实现与运行验证。整理不构成架构决策批准、实现完成或测试通过的证明。
- 整理日期：2026-10-15（UTC）。来源快照包括 Issue 正文与截至 05:58:36 UTC 的全部 3 条评论。
- 目的：一次设备绑定后，从浏览器向 Device 提交编码任务、观察执行、审阅并应用结果；作为实现拆分与验收讨论的单一阅读稿。
- 职责：Device 保管凭据、仓库与完整内容，监管本机 Worker；Server 负责身份、协调、租约与审计；Verifier 在 Device 侧运行验证；MCP 工具宿主按需启动并隔离共享实例。

## 来源与合并口径

1. [Issue #27 原正文：架构设计 v1.5](https://github.com/changw98ic/winwincode/issues/27)：保留第 1—19 节完整技术要求；原文关于“上一轮五项修订落实”的说法是文档复核记录，不代表代码符合设计。
2. [批量调用与委托静态分析，05:13:18 UTC](https://github.com/changw98ic/winwincode/issues/27#issuecomment-6184955369)：纳入附录 A 的实现对照与拆分建议。
3. [分析补充与验证限制，05:25:04 UTC](https://github.com/changw98ic/winwincode/issues/27#issuecomment-6185099637)：修正“围栏等同取消”与未决预算处理，保留生产可达性与基线限制。
4. [远程部署、设备绑定与验收补充，05:58:36 UTC](https://github.com/changw98ic/winwincode/issues/27#issuecomment-6185563151)：按主题并入 §4、§9.1、§9.9、§13、§17。

合并后的新增段落均为待评审设计补充。正文原有要求继续有效；TLS 表述以 §4.2、§13 和 §18.7 的对外安全边界为准，允许受保护的 TLS 终止代理后端，但不放宽远程认证。批量与委托的实现建议以附录 A 为准，其中“围栏不等于完整取消”“未知预算可核对、结算和解除”取代首条评论中的简写。未决参数与实施证据列入附录 B，不在本稿虚构已定方案。

## 总体待办

- [ ] 对照目标代码版本拆分迁移与实现任务，保留身份、权限、取消及预算约束。
- [ ] 跑通提交、执行、验证、审批、应用的一条真实端到端链路。
- [ ] 完成 §17 的故障注入及远程验收，记录固定代码版本、执行结果与证据。

---

## 1. 目标与范围

### 1.1 目标

- 在本机完成一次设备绑定后，用户可在任意位置通过浏览器向该设备提交编码任务、观察执行、审阅并应用结果。
- 代码、对话内容与模型凭据的控制权留在用户设备上。
- 任务执行在进程崩溃、网络断开、消息重复与乱序下保持一致。
- 交付过程可审计，且审计不依赖对内容的访问。
- 单机使用时零配置启动。

### 1.2 非目标（v1）

- Worker 直连 Server。
- 设备离线时的任务排队。
- 外部身份提供方（IdP）接入，作为后续扩展。
- Server 端对内容的搜索、摘要等需要读取明文的功能。
- 按仓库粒度的存放策略，作为后续扩展。

---

## 2. 设计原则

1. **Device 是信任根。** 凭据、仓库绑定与完整内容的权威副本在 Device。
2. **Server 是协调层。** Server 持有映射、状态、租约裁决与审计，不是内容的权威来源。
3. **连接方向单一。** 只有 Device 向 Server 发起外连；Server 不主动连接 Device；Worker 只与本机 Device 通信。
4. **全局栅栏。** 所有改变状态的消息携带 epoch，过期消息在每一层都被拒绝。
5. **先记录后执行，先确认后推进。**
6. **一套机制，三种存放档位。** 档位是配置，不是三套实现。

---

## 3. 角色与信任边界

| 角色 | 职责 | 持有 |
|---|---|---|
| Browser | 界面；用户登录；在加密档下解密内容 | 用户会话；内容主密钥（仅存于浏览器的不可导出密钥存储） |
| Server | 身份与映射；租约裁决；控制事件审计链；同步副本存放；WebSocket 网关 | 用户、设备、仓库、会话之间的映射；任务与租约状态；审计链；按档位存放的内容副本 |
| Device | 信任根；凭据保管；仓库绑定；本地事件日志；拉起并监管 Worker | 模型凭据；设备私钥；内容主密钥；完整内容与工件 |
| Worker | 执行：工作区检出、内核、工具调用、沙箱、上下文管线 | 受控继承的 stdio 管道端点；绑定的执行上下文；仅在内存中的、按需下发的凭据 |
| Verifier（Device 内组件） | 在干净工作区运行验证项，产出签名的结构化验证结果 | 验证配置；设备私钥（用于签名） |
| 工具宿主（Device 内组件） | 托管 MCP 服务器：懒启动、共享、生命周期管理；向调用网关提供工具目录与调用 | MCP 服务器凭据；服务器进程与连接 |

### 3.1 拓扑

```
                     HTTPS / WSS                    WSS（Device 主动外连）
   Browser  ───────────────────────▶  Server  ◀─────────────────────────  Device
      ▲                                  │                                    │
      └──────────────────────────────────┘                                    │ 本机 IPC
              实时事件经 Server 中继                                           ▼
                                                                            Worker
```

### 3.2 可见性矩阵

| 数据 | Browser | Server | Device | Worker |
|---|---|---|---|---|
| 模型凭据 | 否 | 否 | 是 | 仅内存、按需 |
| 映射与元数据 | 是 | 是 | 是 | 否 |
| 控制事件（审计） | 是 | 是 | 是 | 否 |
| 内容（明文同步档） | 是 | 是 | 是 | 是 |
| 内容（加密同步档） | 是 | 仅密文 | 是 | 是 |
| 内容（仅本机档） | 是（在线时） | 仅在中继内存中的密文 | 是 | 是 |

---

## 4. 部署形态

| 项 | local | remote |
|---|---|---|
| 拓扑 | Server、Device、Worker 同一主机 | Server 位于可达位置；Device 位于开发机；Browser 任意 |
| 监听地址 | 仅回环地址 | 任意，由部署决定 |
| TLS | 不要求 | 对外 HTTPS / WSS 必须；受保护代理后端见 4.2 |
| 用户认证 | `local-open` | `password`，或扩展的 `idp` |
| Device↔Server 认证 | 回环内信任，仍使用设备身份 | 设备签名挑战 |
| 内容存放 | 默认 `local_only`；显式配置优先，须满足 `allowed_tiers` | 默认 `encrypted_sync`；显式配置优先，须满足 `allowed_tiers` |

### 4.1 local-open 约束

- Server 在 `local-open` 模式下必须绑定回环地址；绑定非回环地址时拒绝启动。
- 校验 `Host` 与 `Origin`，仅接受回环主机名与预期来源，以防御来自网页的跨站请求与 DNS rebinding。
- Device 与 Worker 按 §13 确认通信对象并绑定执行上下文。

---

### 4.2 远程地址与 TLS 部署（补充建议）

- 区分 Server 的监听地址与供 Browser / Device 使用的对外访问 URL；实现应明确配置入口、优先级，以及 Device 如何获得并保存 Server URL。
- Device 使用对外 HTTPS / WSS 地址，校验证书与主机名；仍仅由 Device 主动外连，Device 不开放供 Server 访问的入站端口。
- 支持反向代理终止 TLS：外部链路必须加密；代理到 Server 可以使用受保护的回环或隔离内网链路，必须明确后端访问限制与可信代理配置。不能仅凭客户端可伪造的转发头认定连接安全。
- 安全约束针对对外入口，不要求应用进程本身一定加载证书。远程入口始终要求用户与设备认证，不能因后端监听回环就按 `local-open` 放行公网请求。

---

## 5. 标识符

| 标识 | 作用域 | 分配方 | 说明 |
|---|---|---|---|
| `user_id` | 全局 | Server | 用户 |
| `device_id` | 全局 | Server | 绑定时分配，与设备公钥一一对应 |
| `repo_id` | 设备内 | Device | 一次仓库绑定 |
| `session_id` | 全局 | Server | 一个连续对话 |
| `task_id` | 会话内 | Server | 一次交付任务 |
| `attempt_id` | 任务内 | Device | 一次执行尝试，单调递增 |
| `lease_id` | 任务内 | Server | 一次租约 |
| `epoch` | 任务内 | Server | 每次授予新租约时加一，单调递增 |
| `seq` | 会话内 | Device | 内容事件序号，单调递增、无空洞 |
| `call_id` | 任务内 | 调用网关 | 一次逻辑调用；由网关在调用点分配；恢复沿用原 `call_id` |
| `candidate_id` | 任务内 | Device | 一个候选改动 |
| `verification_id` | 候选内 | Device | 一次验证运行 |
| `application_id` | 候选内 | Device | 一次应用尝试 |

---

## 6. 数据与存放

### 6.1 数据类别

| 类别 | 内容 | 权威位置 |
|---|---|---|
| A 凭据 | 模型 API 密钥、设备私钥、内容主密钥 | Device |
| B 映射与元数据 | 用户、设备、仓库、会话的对应关系；任务状态；租约；时间戳 | Server |
| C 控制事件 | 任务创建、租约变更、阶段切换、审批、应用、取消、策略变更 | Server（审计链） |
| D 内容事件 | 对话、模型输出、工具调用与结果、上下文裁剪记录 | Device |
| E 工件 | 代码差异、验证证据、日志文件 | Device |

### 6.2 存放档位

| 档位 | D、E 的位置 | Server 可读 | Device 离线时可看历史 |
|---|---|---|---|
| `local_only` | 仅 Device；在线查看时经 Server 加密中继，不落盘 | 否 | 否 |
| `encrypted_sync` | Device + Server（密文） | 否 | 是（需要密钥） |
| `plaintext_sync` | Device + Server（明文） | 是 | 是 |

- A 类数据在任何档位下都不离开 Device。
- B、C 类数据在任何档位下都存放于 Server。

### 6.3 默认与策略

- 未显式配置档位时，`local` 默认使用 `local_only`，`remote` 默认使用 `encrypted_sync`。显式配置优先于部署形态的默认值。
- v1 档位为部署级全局配置。界面必须始终显示当前档位。
- 部署可配置 `allowed_tiers`。显式配置与默认值都必须满足该策略。Device 在绑定与上传前校验，不满足策略时拒绝绑定或停止上传。

### 6.4 档位切换

- 切换产生 `policy.changed` 控制事件，进入审计链。
- 事件身份与存储形态分离（见 7.2）：档位切换与重新加密只改变存储记录，不改变任何已确认事件的 `hash`。
- 低保护切换到高保护（明文→加密）：Device 对历史内容重新加密，向 Server 发送 `storage.replace`，并追加一个 `storage.migrated` 内容事件，记录迁移范围、源形态、目标形态与新的 `storage_digest`。Server 核对替换内容的 `storage_digest` 与已签名的 `storage.migrated` 一致后，才删除旧副本。迁移期间两份副本并存，界面标明。
- 同步档切换到 `local_only`：Device 确认本地副本完整并通过校验后，追加 `storage.migrated`；Server 随后删除同步的内容副本，元数据与 `content_id` 保留。
- 迁移中断时以 `storage.migrated` 是否已被确认为界：已确认则继续清理，未确认则旧副本保持有效并可重新迁移。

---

## 7. 协议

### 7.1 连接

| 连接 | 发起方 | 传输 | 备注 |
|---|---|---|---|
| Browser ↔ Server | Browser | HTTPS / WSS | 用户认证 |
| Device ↔ Server | Device | WSS | 长连接；仅 Device 外连，无需入站端口 |
| Worker ↔ Device | Device 拉起 Worker | Device 创建并独占、继承权限受控的 stdio 管道 | 以管道本身确认通信对象，再绑定执行上下文；可重新连接的 Unix socket 或回环监听须明确连接认证方案（见 §13） |

### 7.2 事件信封

Device 产生的内容事件使用下列信封。

```json
{
  "v": 1,
  "session_id": "...",
  "task_id": "...",
  "attempt_id": 3,
  "epoch": 7,
  "seq": 1024,
  "ts": "2026-01-01T00:00:00Z",
  "kind": "tool.result",
  "prev_hash": "...",
  "content_id": "...",
  "hash": "...",
  "sig": "...",
  "storage": {
    "form": "encrypted",
    "storage_digest": "...",
    "enc": { "alg": "xchacha20poly1305", "key_id": "k1", "nonce": "..." }
  },
  "payload": "..."
}
```

- 事件身份由头部与 `content_id` 构成；存储形态由 `storage` 与 `payload` 构成。二者分离，存储形态可以更换而不改变事件身份。
- `content_id = H(salt ‖ canonical(明文内容))`。盐随内容保存于 Device 本地日志，同步档下一并封装于 `payload` 内；Server 不需要盐。
- `hash = H(prev_hash ‖ canonical(header) ‖ content_id)`，其中 header 为除 `content_id`、`storage`、`payload`、`hash`、`sig` 外的字段。
- `sig` 为设备私钥对 `hash` 的签名。
- `epoch` 为产生该事件时所处的租约 epoch。
- `storage_digest` 为当前存储形态下 `payload` 的摘要，用于传输与存储校验，不参与链。
- 加密 AAD 为 `(session_id, seq, epoch, kind, key_id)`，使密文绑定于其位置。

### 7.3 序号与确认

- Device 为每个会话顺序分配 `seq`，无空洞。
- Device 维护 `sync_watermark`：已被 Server 累计确认的最大 `seq`。
- Server 按序接收，返回累计 `ack(seq)`。
- Device 仅在收到 `ack` 后推进水位；断线重连后从水位之后重放。
- Server 发现空洞时返回 `nack(from_seq)`，Device 从该序号补发。

### 7.4 幂等

- 以 `(session_id, seq)` 去重。
- 相同键且 `hash` 相同：视为重复，直接确认。
- 相同键且 `hash` 不同：拒收，会话标记 `integrity_alert`。
- 存储形态变更（重新加密、迁移）不通过事件上传，使用 `storage.replace`（见 6.4），不改变任何已确认事件的 `hash`。

### 7.5 完整性：两条链

| 链 | 写入者 | 内容 | 校验 |
|---|---|---|---|
| 内容链 | Device | 内容事件的头部与摘要 | Device 签名；Server 保存头部与摘要，不需要读取内容 |
| 控制链 | Server | 控制事件 | Server 签名；Device 保存副本 |

- 控制事件记录当时的 `content_chain_head`；内容事件通过 `cause_id` 引用其响应的控制事件。
- 已被确认的内容无法被 Device 事后改写而不破坏 Server 保存的链；Server 无法伪造 Device 的事件，因为缺少设备私钥。
- 链只依赖头部与 `content_id`，不依赖存储形态，因此三种档位下链的校验方式一致，档位切换不影响已确认事件。

---

## 8. 租约与栅栏

### 8.1 租约

- Server 为任务授予租约 `{lease_id, task_id, device_id, epoch, ttl}`，每次授予 `epoch` 加一。
- Device 在尝试运行期间周期性续约（心跳）。
- 建议默认值：心跳 10 秒，租约有效期 30 秒。
- 租约时间以 Server 时钟为准；Device 使用 Server 下发的时长配合本地单调时钟，不依赖墙钟排序。

### 8.2 栅栏规则

消息按用途分为两条路径，栅栏规则不同。

| 路径 | 内容 | 规则 |
|---|---|---|
| 指令路径 | 租约续约、阶段切换、终态上报、取消确认、应用结果等会改变当前任务状态的消息 | 仅接受当前 `epoch`；过期消息被拒绝，不产生任何效果。过期的应用结果不直接推进状态，其恢复补报见 9.7.1 |
| 日志路径 | 已产生的历史内容事件的补传 | 校验通过即接收（见 8.3）；接收不改变当前任务状态，不触发任何执行 |

各层规则：

| 位置 | 规则 |
|---|---|
| Server | 指令路径拒绝 `epoch` 低于当前租约的消息；日志路径按 8.3 校验 |
| Device | 记录每个任务已见的最大 `epoch`；拒绝低 `epoch` 的 Server 指令；核对 Worker 指令类消息与通道绑定的 `session_id`、`task_id`、`attempt_id`、`epoch`，拒绝非当前尝试或 epoch 的消息 |
| Worker | 每个尝试使用独立的工作区（独立的 git worktree）；仅接受与已确认通道绑定的 `session_id`、`task_id`、`attempt_id`、`epoch` 一致的指令 |

### 8.3 历史日志补传

Server 保存每个任务的租约历史 `{epoch, device_id, granted_at, ended_at, end_reason}`。接收 `epoch` 低于当前租约的内容事件时，要求同时满足：

1. 设备签名与哈希链校验通过；
2. `seq` 与该会话已确认的序号连续；
3. `(task_id, epoch)` 在租约历史中确实授予过该设备。

满足则接收并标记为 `historical`，仅用于补齐内容链与同步副本：不改变任务状态，不续约，不触发指令，也不能使已被新尝试取代的操作重新生效。不满足第 3 条的事件视为完整性异常，会话标记 `integrity_alert`。

`seq` 以会话为单位连续分配，与 `epoch` 无关，因此更换租约不会造成序号空洞。

### 8.4 租约丧失

- Device 无法续约且超过宽限期（建议等于租约有效期）时，必须终止 Worker。
- Server 在租约过期后将任务置为 `suspended`，不得再接受该 `epoch` 的状态变更。

---

## 9. 完整链路

### 9.1 设备绑定

```
Device   首次启动，生成设备密钥对（Ed25519）
Browser  已登录用户请求创建配对
Server   签发一次性 pairing_token（TTL 10 分钟），返回给 Browser 展示
User     在 Device 上输入 pairing_token
Device   携带 token 与设备公钥向 Server 注册
Server   校验并销毁 token；分配 device_id；建立 user↔device 映射；写 device.bound 控制事件
```

加密档与 `local_only` 档的密钥配对：

```
Browser ⇄ Device   经 Server 中继完成 X25519 密钥交换（Server 只转发）
双方               显示由交换记录派生的短验证码（SAS），用户确认一致
Device → Browser   以交换所得密钥封装内容主密钥并发送
Browser            将内容主密钥存入不可导出的密钥存储
```

- 首次绑定时由 Device 生成内容主密钥，并可生成一次性展示的恢复密钥。
- 新增设备或新增浏览器，需由已被信任的端点通过同一流程下发内容主密钥。

#### 9.1.1 绑定入口与持久化（补充建议）

- Browser 应明确已登录用户在哪里生成一次性配对信息，并展示有效期；沿用 §9.1 的 10 分钟 TTL。
- Device 应明确 Server URL 与配对令牌的输入方式，以及绑定成功或失败的展示方式。
- 发起绑定前，Device 持久化设备密钥和稳定的绑定请求 ID；成功后至少保存 Server URL、`device_id`、设备私钥的安全存储引用及必要绑定状态。
- Server 保存用户、设备、公钥的绑定关系与绑定结果。一次性配对令牌不作为后续登录凭据；已绑定 Device 重启后使用原设备身份认证，不重复配对。

#### 9.1.2 绑定请求重试与响应丢失（补充建议）

覆盖“Server 已消费令牌并完成绑定，但成功响应丢失”：

1. Server 原子地完成令牌消费、设备绑定与幂等结果记录；§9.1 的绑定流程按此原子边界实现。
2. 相同绑定请求 ID、相同公钥及相同绑定意图的重试返回原绑定结果，不创建第二台设备。
3. 同一请求 ID 携带不同公钥或不同绑定意图时拒绝。
4. 恢复须证明持有原设备私钥，不能只凭请求 ID 获取绑定信息或恢复授权。
5. 实现须明确恢复记录的保留范围、过期后的处理；已吊销绑定不能通过重试重新激活。具体保留期限与过期交互尚待设计确认。

### 9.2 提交与下发

```
Browser → Server   提交任务 {session_id, device_id, repo_id, 内容（按档位明文或密文）}
Server             校验权限与设备在线
Server             写控制事件 task.created
Server             创建租约，epoch 加一，写 lease.granted
Server → Device    下发任务，携带 epoch 与 cause_id
```

### 9.3 本地执行

```
Device             校验 epoch 与指令；解密内容；追加本地事件日志（分配 seq）
Device             分配 attempt_id；创建独立 worktree
Device             创建并独占 stdio 管道；控制继承权限，拉起 Worker
Device ↔ Worker    以管道的创建、独占与受控继承关系确认对端，绑定 {session_id, task_id, attempt_id, epoch}
Device → Worker    经已绑定通道传递任务、按需凭据（仅内存）
Worker             组装上下文（见第 12 节）→ 调用模型 → 调用工具
                   每次调用先写 request 与 pending，完成后写配对的 result
Worker → Device    事件流；Device 写入本地日志
```

### 9.4 同步

```
Device → Server    事件信封（见 7.2）
Server             校验签名、序号与链；校验租约归属（低于当前 epoch 的事件按 8.3 作为历史日志接收）；
                   记录头部与 content_id；按档位存放 payload
Server → Device    ack(seq)
Device             推进 sync_watermark
Server → Browser   向订阅该会话的浏览器推送（加密档下为密文）
```

### 9.5 交付流程（StrongFlow）

```
需求 → 方案 → 执行 → 评审 → 验证 → 候选改动 → 审批 → 应用
```

- 阶段切换由 Server 裁决并写入控制链；阶段产物（方案、差异、评审意见、验证证据）留在 Device，Server 只持有阶段状态、内容摘要与验证结果。
- 验证由 Device 上的 Verifier 运行，不依赖模型自述（见 9.5.1）。未取得有效且通过的验证结果，任务不得进入候选改动状态。
- 候选改动记为 `{candidate_id, repo_id, base_commit, candidate_digest}`，其中 `base_commit` 是候选所基于的仓库提交。

#### 9.5.1 验证

- Verifier 在由 `base_commit` 加候选差异构成的干净工作区中运行策略规定的验证项（构建、测试、静态检查等）。
- 产出由设备私钥签名的结构化结果：

```json
{
  "verification_id": "...",
  "candidate_id": "...",
  "base_commit": "...",
  "candidate_digest": "...",
  "checks": [
    { "id": "test", "cmd_digest": "...", "exit_code": 0, "status": "pass", "evidence_digest": "..." }
  ],
  "verdict": "pass",
  "sig": "..."
}
```

- Server 校验：签名有效；`candidate_id`、`base_commit`、`candidate_digest` 与当前候选一致；策略要求的 `required_checks` 全部出现且 `status` 为 `pass`；`verdict` 为 `pass`。任一不满足，任务不得进入 `candidate_ready`。
- `evidence_digest` 仅用于关联证据，不替代 `status` 与 `verdict`。
- 这是设备证明，不是服务端证明：Server 不重新运行验证。
- `required_checks` 由部署策略配置，默认至少一项；允许显式配置为空，该配置记入审计链。

### 9.6 浏览器查看

| 场景 | 行为 |
|---|---|
| Device 在线 | 实时事件经 Server 中继；加密档与 `local_only` 档下由 Browser 解密 |
| Device 离线，同步档 | 从 Server 读取同步副本，界面为只读，输入被禁用并提示设备离线 |
| Device 离线，`local_only` 档 | 仅显示映射、状态与时间线等元数据，内容不可见 |

- 搜索与摘要等需要明文的功能仅在 Browser 端解密后本地执行。

### 9.7 审批与应用

```
Browser → Server   批准 {candidate_id}
Server             写控制事件 approval.granted，绑定 {repo_id, base_commit, candidate_digest}
Server → Device    下发应用指令，携带 approval_id、application_id 与上述绑定
Device             核对：目标分支 HEAD 等于 base_commit，工作树干净，候选摘要一致
                   任一不满足 → 候选标记 stale，不应用
Device             写 application.started{application_id} 并落盘，先于任何仓库写入
Device             在绑定的本地仓库应用并提交；提交信息带尾注 WinWinCode-Application: <application_id>
Device             写 application.finished{application_id, result_commit}
Device → Server    回报 apply.result；Server 按 application_id 幂等记录并写入控制链
```

- 审批绑定 `{repo_id, base_commit, candidate_digest}`，而不仅是差异摘要：同一份差异应用到不同基线，结果可能不同。
- 基线已变化时，候选标记为 `stale`，不自动合并或变基；任务回到执行阶段，基于新基线重新生成候选，并重新验证与审批。
- 仓库的写入仅由 Device 执行，Server 与 Worker 不直接修改用户仓库。

#### 9.7.1 应用的崩溃恢复

Device 启动时扫描存在 `application.started` 而无 `application.finished` 的应用：

| 仓库状态 | 处理 |
|---|---|
| 目标分支自 `base_commit` 起存在带该 `application_id` 尾注的提交 | 补写 `application.finished`（`result_commit`）并回报，不重复提交 |
| 无此提交，HEAD 仍为 `base_commit`，工作树无残留 | 视为未开始，可使用同一 `application_id` 重试 |
| 其他情形（HEAD 已变化，或存在部分应用的残留） | 记为 `outcome_unknown`，任务进入 `blocked`，由用户决定 |

Server 以 `application_id` 幂等接收，重复回报直接确认。

**恢复补报与 epoch**

若崩溃后租约已更换，旧 `epoch` 下的 `apply.result` 不能直接推进状态；同时不放开其他旧 `epoch` 的应用结果。流程为：

1. 旧记录（`application.started`、`application.finished`）按历史事实经日志路径补传（见 8.3），不改变任务状态。
2. Device 重连后，Server 为该任务授予恢复租约（`epoch` 加一）。Device 在该授权下提交 `application.reconciled`，内容为核对结果（已应用并给出 `result_commit`、未开始，或 `outcome_unknown`），并关联原 `application_id`、`approval_id` 与原 `epoch`。
3. Server 核对：`application_id` 确由该审批下发给该设备；`base_commit` 与 `candidate_digest` 与审批绑定一致；该 `application_id` 尚未被记录为已完成。通过后由 Server 裁决状态转换。
4. 若任务在此期间已进入终态（例如已取消），事实仍记入控制链（提交已存在于仓库）并提示用户，不改变该终态。

### 9.8 取消

```
Browser → Server → Device → Worker   逐级传递，逐级确认
Worker  → Device → Server            确认已停止
Server                               按当前 epoch 裁决终态 cancelled，写控制事件
```

- 设备离线时，Server 记录 `cancel_requested`，在设备重连后投递；租约过期同样会使 Device 终止 Worker。

### 9.9 断线与恢复

```
Device 重连 Server，发送 {session_id, 最后确认 seq, 运行中的尝试及其 epoch}
Server 比对租约：
  epoch 为当前且租约未吊销 → 续约，尝试继续
  否则                     → 指令 Device 终止该尝试
Device 从 sync_watermark 之后重放事件（低于当前 epoch 的事件按 8.3 作为历史日志接收）
```

---

### 9.10 重连失败与设备吊销（补充建议）

- 临时断网按退避策略自动重连；已绑定设备重启不重新配对。
- 区分临时连接失败、凭据无效与设备已吊销；界面 / CLI 明确展示原因；永久认证失败停止无效重试。
- 吊销不只影响下一次登录：Server 同时使已有连接和相关租约失效，拒绝继续接受状态变更。
- 在线 Device 收到吊销或确认授权失效后，停止相关 Worker；涉及子线程及在途请求时，参见附录 A 的取消边界。
- 离线 Device 无法保证立即获知吊销，应依靠 §8.4 的本地租约到期机制，在明确的时间上限内停止；恢复连接后不得继续旧任务，也不自动重新绑定。
- 验收分别证明“旧结果不能推进状态”与“Worker / 子进程确实停止”。拒绝状态写入不等于已经停止计算或计费。
- 上述限制不把历史日志转换为执行授权：§8.3 的历史日志规则继续有效；§9.7.1 的应用核对补报仍须在当前授权下提交。

---

## 10. 执行日志与崩溃恢复

### 10.1 日志

- 每次模型调用与工具调用先持久化 `request` 与 `pending`，完成后持久化配对的 `result`。
- 凭据不进入事件日志；日志写入前进行脱敏。

### 10.2 重试分类

每个调用类型（工具、HTTP 请求、模型调用）在调用网关（见第 11 节）注册时声明重试类别，按“重试是否可能造成重复损害”划分。未声明的按 R1 处理；HTTP 请求默认 `GET`、`HEAD` 为 R0，写类方法（`POST`、`PUT`、`PATCH`、`DELETE`）为 R1，可按目标显式覆盖。

| 类别 | 判定 | 例子 |
|---|---|---|
| R0 可直接重试 | 只读；或效果完全限定在当前尝试的隔离工作区、随尝试一并丢弃 | 读文件、搜索、在隔离 worktree 内的编辑与构建 |
| R1 核对后重试 | 重试可能造成重复损害 | 工作区之外的写入、网络写入、发布；本地提交；文件删除；可能重复计费的模型调用 |

### 10.3 崩溃后的处理

Device 重启后扫描日志，对存在 `pending` 而无 `result` 的调用：

- R0：记为 `failed(interrupted)`，可由新尝试重试。
- R1：记为 `outcome_unknown`，不得当作失败。

`outcome_unknown` 的解除顺序：

1. 调用带幂等键：使用 §11.1 规定的同一键重试，跨尝试或租约恢复时保持该键不变。
2. 可核对实际结果：查询真实状态（提交对象是否存在、文件是否已删除、远端是否已有该写入），据此补记 `result`，或确认未发生后重试。
3. 无法核对：任务进入 `blocked`，由用户决定，决定写入日志。

模型调用的 `outcome_unknown` 以任务预算为界，不要求人工确认：

- 提供方支持幂等键或可查询结果：按 1 或 2 处理。
- 两者都不支持：将该未知调用的成本上限计入已用预算；若剩余预算仍足以重新调用，则重新调用并记录 `possible_duplicate_charge`。
- 剩余预算不足：进入 `blocked`。

### 10.4 重复发起的拦截

Device 禁止把处于 `outcome_unknown` 的 R1 调用作为**新调用**重复执行，包括新尝试经重新规划后发出的相同调用；同一个逻辑调用（相同的 `session_id`、`task_id`、`call_id`）按 10.3 恢复不受此限。

- 调用由 `(session_id, task_id, call_id)` 唯一标识。恢复沿用原 `session_id`、`task_id`、`call_id`，并记录其所属的原 `attempt_id`；Code Mode 中复用原 `call_id` 的条件见 11.1。
- `(工具, 规范化参数摘要)` 只用于识别疑似重复：命中且调用身份不同的新调用，在原调用的 `outcome_unknown` 解除前被拦截；它不能代替调用身份，也不会拦截按 10.3 进行的恢复。

### 10.5 恢复

恢复通过新的尝试完成：`attempt_id` 加一，申请新租约（`epoch` 加一），使用新的隔离工作区，由新尝试依据日志重新规划。恢复同一逻辑调用时，沿用原 `session_id`、`task_id`、`call_id` 与幂等键；新的 `attempt_id`、`epoch` 用于执行上下文与围栏，不进入幂等键。

---

## 11. 调用网关与配置

### 11.1 调用网关

所有出站调用统一经过调用网关：工具调用、HTTP 请求、模型调用，以及 Code Mode 中执行的代码发出的调用。网关是 Worker 调用路径上的逻辑层，调用在执行前经 Device 落盘（见 10.1）。

职责：

- **调用身份**：`call_id` 由网关在调用点分配，不由调用方（包括 Code Mode 中的代码）提供。
- **重试分类与幂等**：按 10.2 归类；目标支持幂等键的请求携带确定性幂等键 `H(canonical([session_id, task_id, call_id]))`。数组按 `session_id`、`task_id`、`call_id` 的固定顺序构造，并使用统一的规范编码。恢复同一逻辑调用时沿用这三个标识；跨 `attempt_id`、`epoch` 恢复仍使用原键。`attempt_id`、`epoch` 用于执行上下文与围栏，不进入幂等键。
- **日志**：执行前落盘 `request` 与 `pending`，完成后落盘 `result`。
- **策略**：超时、重试次数与退避、并发上限、出站目标白名单、预算。
- **凭据**：按调用类型注入所需凭据，日志中脱敏。

约束：

- Code Mode 内的调用与普通调用走同一网关、同一日志、同一分类，不得存在绕过网关的出站路径。
- Code Mode 的候选调用身份由代码块标识与块内序号构成。序号相同不代表调用相同：复用原 `call_id`（及其幂等键与恢复豁免）前，网关必须核对原 `request` 记录的工具、目标与规范化参数摘要。一致才复用；不一致则分配新的 `call_id`，视为新调用，走 10.4 的重复检查。
- 网关治理到调用粒度。工具内部（例如一条 shell 命令）发出的网络访问不逐条治理，整体按一次工具调用归类，并由沙箱的网络策略约束。

### 11.2 配置解析

目标：局部配置与全局配置的冲突不得在运行中终止任务。

- **层级**（低到高）：部署默认 < 设备 < 仓库 < 任务；以上四层在任务启动时固化为任务级快照。单次调用覆盖在调用时提供（见下）。
- **约束类配置项**（出站白名单、预算上限、必需验证项、允许的存放档位）：下层只能收紧，不能放宽上层。
- **行为类配置项**（超时、重试次数、退避、并发）：下层覆盖上层，且必须落在上层声明的允许范围内。
- **单一解析点**：配置只在网关解析；Worker、Verifier 等组件使用解析后的值，不各自读取配置。
- **启动前校验**：任务启动时一次性解析并校验前四层，生成任务级配置快照（包含启用的工具源及其版本摘要），`config_digest` 记入任务启动事件。冲突或越界在此刻拒绝启动，并指出冲突项及其来源层级。
- **运行中不重新解析**：任务级快照在运行中不变；配置变更只对新任务生效。快照经校验保证完整且无矛盾，因此前四层在运行中不存在需要再裁决的冲突。
- **单次调用覆盖**：调用方（含 Code Mode 中的代码）可在调用时指定局部覆盖。覆盖由网关在调用时对照任务级快照校验，规则同上（约束类只能收紧，行为类须在允许范围内）。通过校验的覆盖随该调用的 `request` 事件记录；无效覆盖只使该调用被拒绝并返回错误，不终止任务，也不改变快照。
- **可观测**：每个调用的 `request` 事件携带 `config_digest` 与生效的局部覆盖，用于定位该调用行为由哪份配置决定。

### 11.3 工具源：MCP 与 Skill

MCP 工具与 Skill 统一作为工具源，经调用网关暴露给 Worker 与 Code Mode。

- **目录与按需加载**：Device 维护工具目录（名称、描述、来源、版本摘要）。任务启动时目录随配置快照固定（见 11.2）；工具的完整 schema 与 Skill 内容在首次使用时按需加载，不在启动时全部载入。
- **宿主托管，懒启动**：MCP 服务器由 Device 上的工具宿主托管，不随 Worker 进程各自启动；首次调用时启动，空闲超时后停止。Worker 经本机 IPC 通过网关访问，不直接连接 MCP 服务器。
- **共享键与隔离**：同一共享键下的多个 Worker、尝试与任务复用同一实例。目录中为每个服务器声明共享方式：

  | 声明 | 共享键 | 适用 |
  |---|---|---|
  | `stateless` | `(server_id, 凭据范围)` | 无状态、不依赖工作目录的服务器 |
  | `per_repo` | `(server_id, repo_id, 凭据范围)` | 绑定工作目录或仓库根的服务器 |
  | `per_session` | `(server_id, repo_id, session_id, 凭据范围)` | 持有会话状态的服务器 |

  未声明的服务器按 `per_session` 处理。同一会话可以涉及不同仓库；不同 `repo_id` 不共享 `per_repo` 或 `per_session` 实例。工作目录等上下文随调用传入，不依赖实例的启动参数。
- **可选，而非强制**：某个 MCP 服务器或 Skill 是否启用由配置决定，不是任务启动的前提。服务器不可用不终止任务，按下列情形处理：
  - 请求尚未发送到服务器（启动失败、连接失败）：确认未执行，对应调用返回失败，给模型或代码。
  - 请求已发出而结果不明（服务器在此后崩溃或断开）：按 10.3 处理，R0 记为 `failed(interrupted)`，R1 记为 `outcome_unknown`。
  - 工具宿主可按退避重启服务器，但重启不得重放结果不明的调用。
- **调用与分类**：MCP 工具调用走网关，重试类别以目录中的显式声明为准。MCP 工具注解（`readOnlyHint`、`idempotentHint`、`destructiveHint`）仅作为受信服务器的默认值，不作为信任依据；未声明按 R1。
- **凭据**：MCP 服务器所需凭据保存在 Device，按服务器范围注入，不进入日志与模型上下文，也不与模型凭据共用。
- **Skill**：作为在 Worker 沙箱内执行的代码，其内部发出的调用同样经网关；Skill 的标识与内容摘要（`skill_digest`）记入调用事件，用于审计与恢复核对。
- **实例状态**：`declared`、`starting`、`ready`、`idle`、`stopped`、`failed`。

---

## 12. 上下文管线（Worker 内部）

上下文管线位于 Worker 内部，不改变协议，仅通过事件对外可见。

| 组件 | 职责 |
|---|---|
| Fusion | 多模型聚合 |
| JEV | 决策模型，判断上下文条目保留或删除 |

约束：

- JEV 的每次决策以 `context.prune` 事件记录：条目标识、条目摘要、决策、原因、预估收益与预估缓存损失。
- 裁剪策略需考虑前缀缓存：达到阈值后批量裁剪，优先裁剪靠后的内容，仅在预估收益大于缓存损失时执行。
- 管线配置（是否启用 JEV、是否启用 Fusion）作为尝试的参数记录在事件中，用于对照评测。

---

## 13. 认证与传输安全

| 通道 | 认证 | 传输保护 |
|---|---|---|
| Browser ↔ Server | `local-open`（仅本地回环入口）/ `password` / 扩展 `idp` | 远程对外链路必须 TLS；代理后端见 4.2 |
| Device ↔ Server | 设备私钥对挑战签名 | 远程对外链路必须 TLS；可选 mTLS；代理后端见 4.2 |
| Worker ↔ Device | 以 Device 创建并独占、继承权限受控的 stdio 管道本身确认通信对象，再绑定 `session_id`、`task_id`、`attempt_id`、`epoch` | 本机 IPC，无需 TLS；可重新连接的 Unix socket 或回环监听须明确连接认证方案 |
| 模型凭据 | 保存于系统钥匙串或权限为 0600 的文件 | 仅在内存中传递给 Worker，按需、按尝试 |
| MCP 服务器凭据 | 同上，按服务器分别保管 | 仅在启动对应服务器时由工具宿主注入，不传递给 Worker |

规则：

- 仅本地回环入口可使用不要求 TLS 与用户认证的 `local-open`，仍须遵守 §4.1；反向代理的回环后端不是远程入口免认证的依据。
- 远程对外入口必须具备有效 HTTPS / WSS 安全边界；应用进程直接终止 TLS 或按 §4.2 正确配置 TLS 终止代理均可。缺少该安全边界时拒绝以远程模式启动或暴露入口；不再一概要求 Server 应用进程加载证书。
- Device 校验证书与主机名，拒绝连接非 TLS 的非回环 Server；仅信任明确配置且受访问控制保护的代理，不能信任任意转发头。
- Device 创建并独占 Worker 的 stdio 管道，控制管道端点的继承权限。Device 与 Worker 以管道本身确认通信对象，再绑定 `session_id`、`task_id`、`attempt_id`、`epoch`，并按 §8.2 校验指令。
- 使用可重新连接的 Unix socket 或回环监听时，必须明确连接认证方案；执行上下文标识用于绑定与围栏，不能代替连接认证。
- 设备被吊销后，Server 拒绝新连接、使已有连接及其全部租约失效，并拒绝继续接受状态变更；在线停止与离线限时停止按 §9.10 执行。

---

## 14. 内容加密与密钥

### 14.1 密钥层级

```
内容主密钥（CMK，256 位，用户级）
  └─ 会话数据密钥（DEK，每会话随机生成，由 CMK 封装，封装结果存于会话头事件）
       └─ 事件加密（AEAD，AAD 绑定位置）
```

### 14.2 存放

| 密钥 | 位置 |
|---|---|
| CMK | Device；Browser 的不可导出密钥存储；可选的用户保管的恢复密钥 |
| DEK | 以封装形式随会话头事件存放 |
| 设备私钥 | Device，仅本机 |

### 14.3 轮换与丢失

- 信封携带 `key_id`，轮换时创建新的 CMK 版本，旧 DEK 惰性重新封装。
- 浏览器密钥丢失：由已信任端点重新配对。
- 所有端点丢失且无恢复密钥：加密档的历史内容不可恢复。
- 设备丢失或凭据泄露：Server 吊销 `device_id`，用户重新绑定并轮换 CMK。

---

## 15. 状态机

### 15.1 任务

| 状态 | 含义 | 可转入 |
|---|---|---|
| `created` | 已创建 | `queued`、`cancelled` |
| `queued` | 等待设备 | `leased`、`cancelled` |
| `leased` | 已授予租约 | `running`、`suspended`、`cancelled` |
| `running` | 尝试执行中 | `candidate_ready`、`blocked`、`failed`、`suspended`、`cancelled` |
| `suspended` | 设备离线或租约过期 | `leased`（续约或重新授予）、`cancelled`；存在未完成的应用时，在恢复租约下经 `application.reconciled` 转入 `applied`、`approved`、`blocked` |
| `blocked` | 存在无法自动解除的 `outcome_unknown`，等待用户决定 | `running`、`applied`、`failed`、`cancelled` |
| `candidate_ready` | 候选改动与通过的验证结果就绪 | `approved`、`rejected`、`stale` |
| `stale` | 候选的基线已变化 | `running`（基于新基线重新生成）、`cancelled` |
| `approved` | 已审批 | `applying`、`stale`、`cancelled` |
| `applying` | 正在应用 | `applied`、`suspended`、`blocked`、`failed` |
| `applied` / `rejected` / `failed` / `cancelled` | 终态 | — |

`applying` 期间设备离线或租约过期时，任务进入 `suspended`。重连后 Server 授予恢复租约，Device 提交 `application.reconciled`（见 9.7.1），Server 按核对结果转换：已应用 → `applied`；未开始 → `approved`（可使用同一 `application_id` 重试）；结果不明 → `blocked`。若任务已处于终态，只记录事实，不改变终态。

### 15.2 设备

| 状态 | 含义 |
|---|---|
| `bound_offline` | 已绑定，无活动连接 |
| `online` | 连接正常，心跳有效 |
| `degraded` | 在线，但同步积压超过阈值 |
| `revoked` | 已吊销 |

---

## 16. 故障矩阵

| 场景 | 处理 |
|---|---|
| Worker 崩溃 | Device 记录 `attempt.failed`，申请新租约（epoch+1），启动新尝试 |
| Device 崩溃或重启 | 读取本地日志，按 10.3 处理未完成调用（R0 记为 `failed(interrupted)`，R1 记为 `outcome_unknown`）；重连后上报，由 Server 裁决 |
| Server 重启 | 从持久化状态恢复；Device 自动重连并按水位重放 |
| 网络分区 | 续约失败超过宽限期则终止 Worker；本地日志保持完整；恢复后重放 |
| 消息重复或乱序 | 以 `(session_id, seq)` 去重；空洞触发 `nack` 补发 |
| 旧 Worker 迟到消息 | `attempt_id` 或 `epoch` 不匹配，丢弃并记录 |
| 链不一致 | 拒收；会话标记 `integrity_alert`，需人工处理 |
| 时钟偏差 | 不使用墙钟排序，仅使用 `seq`、`epoch` 与 Server 时钟下的租约 |
| 设备丢失 | 吊销设备；租约失效；重新绑定并轮换 CMK |
| 断线期间写入的旧 epoch 事件 | 作为历史日志补传，按 8.3 校验后接收，不改变任务状态 |
| 不确定的工具调用（R1） | 记为 `outcome_unknown`，核对或交由用户；期间拦截作为新调用发起的同一调用，同一 `call_id` 按 10.3 恢复不受限 |
| 应用提交后、记录结果前崩溃 | 按 9.7.1 以 `application_id` 尾注核对，补记结果，不重复提交；租约已更换时经恢复租约下的 `application.reconciled` 补报 |
| 基线在审批后变化 | 候选标记 `stale`，重新生成并重新验证、审批 |
| 档位迁移中断 | 以 `storage.migrated` 是否已确认为界，旧副本保持有效至确认 |
| MCP 服务器启动失败 | 请求尚未发送，对应调用返回失败，任务继续；工具宿主按退避重启 |
| MCP 服务器在请求发出后崩溃 | 结果不明，按 10.3 处理（R0 记 `failed(interrupted)`，R1 记 `outcome_unknown`）；重启不重放结果不明的调用 |

---

## 17. 可观测、评测与验收

每次尝试记录并可按任务汇总：

- 通过率、返工次数；
- 输入、输出与缓存命中的 token 数及成本；
- 耗时；
- 上下文管线配置（基线 / JEV / JEV+Fusion）。

对同一批任务在不同配置下运行并对比这些指标，作为 JEV 与 Fusion 效果的依据。

端到端验收：

- 注入一个必然失败的验证项，交付必须被拦截。
- 在审批后改变仓库基线，候选必须被标记为 `stale`。
- 在应用提交后、记录结果前强制崩溃：恢复后不得产生重复提交；若租约已更换，仍能通过 `application.reconciled` 补报并正确推进状态。
- 对 `outcome_unknown` 的 R1 调用，同一逻辑调用身份的恢复可以执行，新尝试发出的同参数新调用被拦截。
- 对支持幂等键的目标，核对请求使用 `H(canonical([session_id, task_id, call_id]))`，字段顺序与规范编码固定；同一逻辑调用跨 `attempt_id`、`epoch` 恢复后键不变。不同会话即使 `task_id`、`call_id` 相同，也必须得到不同的幂等键。
- 未显式配置时，`local` 使用 `local_only`，`remote` 使用 `encrypted_sync`；显式配置优先，显式配置与默认值都须满足 `allowed_tiers`。
- Device 创建并独占、继承权限受控的 stdio 管道；Device 与 Worker 以管道本身确认通信对象并绑定执行上下文。与通道绑定的 `session_id`、`task_id`、`attempt_id` 或 `epoch` 不符的指令被拒绝；可重新连接的 Unix socket 或回环监听须验证其明确的连接认证方案。
- 给出相互冲突的任务级配置层，任务必须在启动前被拒绝并指出冲突项，而不是在运行中终止；任务运行期间修改配置，不影响该任务。
- 调用时给出越界的局部覆盖，只有该调用被拒绝，任务继续运行。
- Code Mode 代码块恢复执行时，同一位置的调用若工具、目标或参数摘要与原 `request` 不一致，必须分配新的 `call_id`，不得复用原身份与幂等键。
- Code Mode 中的代码发出的 HTTP 请求与工具调用，必须出现在同一调用日志中，并带有网关分配的 `call_id`。
- 某个 MCP 服务器无法启动时，任务继续运行，对应调用返回错误，不终止任务。
- MCP 服务器在 R1 调用发出后崩溃：该调用记为 `outcome_unknown`，服务器重启后不得被重放。
- 两个不同仓库的任务，不得共享声明为 `per_repo` 或 `per_session` 的 MCP 实例；同一 `session_id` 下的两个不同 `repo_id` 也不得共享这些实例。
- 在断线期间写入旧 epoch 事件，恢复后会话不得卡住，且任务状态不被改变。

---

### 17.1 最小远程验收（补充建议）

使用两台机器：Server 在一台，Device 与 Worker 在另一台；Device 不开放任何供 Server 访问的入站端口。

- [ ] 经对外 HTTPS/WSS 地址完成一次绑定，任务下发、执行与结果回传。
- [ ] 经反向代理终止 TLS 的部署完成相同链路，后端不能绕过认证。
- [ ] Server 完成绑定后丢弃响应；Device 重试恢复原绑定，不产生重复设备，也不要求重新配对。
- [ ] 已绑定 Device 重启后自动恢复认证和连接。
- [ ] 网络中断后重连，按已有 ACK/恢复规则处理，不重复执行已完成的副作用。
- [ ] 任务执行中吊销设备：连接和租约失效，在线 Worker 实际停止，迟到状态变更被拒绝。
- [ ] Device 离线期间吊销：本地租约到期后停止，重连明确显示已吊销，不自动重新绑定。
- [ ] 记录固定代码版本、两端日志、绑定记录、任务结果和进程退出证据。


### 17.2 批量与委托故障注入（补充建议）

- [ ] 批内各成员分别验证 R0 / R1 恢复；取消或崩溃后的已发出 R1 不遗留无解释的 `pending`，而记为 `outcome_unknown`，按 §10 恢复。
- [ ] 普通调用与 Code Mode 的并发控制确实共享；模型批次在派发前预留预算，父子任务预算不会重复可用或超额发放。
- [ ] 委托结果未知时不重新 spawn；证明核对结果、预算结算 / 解除规则与日志一致。
- [ ] 取消分别检查父子谱系的迟到状态被围栏拒绝，以及子线程、子进程和可中断在途请求实际停止；不能把围栏证据当成完整取消证据。
- [ ] Code Mode 重启后若 cell 映射已丢失、出现 `cell_scope_expired`，不伪造恢复身份或绕过 §10.4 / §11.1 的核对。
- [ ] 同一 MCP 服务器有 k 个并发 R1 在途时崩溃，k 个均记为 `outcome_unknown`，宿主重启不重放。
- [ ] 对实际生产装配核实普通工具、Code Mode、模型、HTTP、子 agent 及 `codex_delegate` 的网关与权限路径；后者的生产可达性在原分析中尚未验证，须先确认路径再得出缺陷结论。

以上均为待执行验收，不表示场景已实现或通过测试。

---

## 18. 架构不变量

以下约束不得在未经架构决策记录（ADR）的情况下改变。

1. 模型凭据、MCP 服务器凭据与设备私钥不离开 Device。
2. 连接只由 Device 向 Server 发起；Server 不主动连接 Device。
3. Worker 仅与本机 Device 通信，不直连 Server。
4. 所有改变状态的消息携带 `epoch`，每一层都拒绝过期消息。
5. 仓库只由 Device 写入，且仅在存在绑定了 `{repo_id, base_commit, candidate_digest}` 的审批之后；应用前必须核对基线，基线变化时不自动合并。
6. Server 不持有内容的权威副本；内容的权威副本在 Device。
7. 远程对外通信必须使用 TLS；允许 §4.2 规定的受保护 TLS 终止代理后端；`local-open` 仅允许本地回环入口，不得使远程入口绕过用户与设备认证。此处为远程评论提出的待评审边界澄清，不代表已有 ADR 批准。
8. 内容链与控制链分别由 Device 与 Server 写入并签名。
9. 存放档位是配置，不得引入按档位分叉的执行路径。
10. 模型与工具调用先持久化请求再执行；崩溃后按重试分类处理，R1 调用在结果核对前记为 `outcome_unknown`，不得当作失败，且不得作为新调用重复执行；同一逻辑调用只能按 10.3 的顺序恢复。
11. 日志与事件不包含凭据。
12. 低于当前 `epoch` 的历史日志可以补传，但不得改变当前任务状态、续约或触发执行；应用结果的恢复补报须在当前授权下以核对结果提交，由 Server 裁决。
13. 事件身份（`hash`）不依赖存储形态；重新加密与档位迁移不改变已确认事件。
14. 验证由 Device 上的 Verifier 运行并签名，产出结构化结果；Server 以明确的结果字段放行，摘要只用于关联证据。
15. 应用必须先落盘 `application.started`，并以 `application_id` 保证幂等与可恢复。
16. 所有出站调用（工具、HTTP、模型，含 Code Mode）经同一调用网关；配置只在网关解析，任务按启动时的任务级配置快照执行；调用级覆盖在调用时校验，无效覆盖只拒绝该调用。
17. MCP 服务器由 Device 的工具宿主托管并按共享键隔离，不随 Worker 进程各自启动；服务器不可用不得终止任务。

---

## 19. 版本演进

- 信封与协议携带版本号 `v`；新增字段向后兼容，未知的事件类型被保留并纳入链，但不被解释。
- 扩展项：远程 Worker 直连（须使用租约绑定的短期令牌，且不下发长期凭据）、设备离线任务排队、外部 IdP、按仓库的存放策略。

---

## 附录 A. 批量调用与委托：实现对照与建议

### A.1 证据范围与优先级

本附录合并 [05:13 静态分析](https://github.com/changw98ic/winwincode/issues/27#issuecomment-6184955369) 与 [05:25 补充意见](https://github.com/changw98ic/winwincode/issues/27#issuecomment-6185099637)。原分析基于 `main@4c58ecd5` 静态阅读，未运行测试；此前 PR #26 对照基线为 `5ea61ef84f56410dde9c307832feb61cb23a0bd1`。实施前必须在目标提交重新核对代码位置、生产装配、调用链及已有约束，不能将旧基线观察当作最新代码已证实缺陷。

方向是优先复用现有工具派发入口、调用账本与恢复机制，在已有路径补齐语义，不另造一套。先解决重复执行、超预算和取消不彻底的实际风险，再按恢复与排障需要补批次记录、统一日志视图等结构。以下为提议，不是已接受的排期或完成声明。

### A.2 原静态分析中的实现观察（均待目标版本复核）

`cx/` 指 `third_party/codex/codex-rs/`。表中是原评论所报告的静态观察，不表示本整理新增了代码检查或运行证据。

| 路径 | 原报告位置 | 原静态观察与限制 |
|---|---|---|
| 一轮多个工具调用 | `cx/core/src/session/turn.rs`、`tools/parallel.rs` | 以 `FuturesOrdered` 并发执行、按模型顺序提交；每调用有独立账本，批本身无记录；观察到读写锁而未见并发上限，需核实实际装配约束 |
| Code Mode 嵌套调用 | `code_mode/delegate.rs`、`code_mode/mod.rs` | 嵌套调用各自 `tokio::spawn`，走相同派发入口但另建 `ToolCallRuntime`；原分析未见与直接调用共享并行锁或上限 |
| 并行模型调用 | `parallel_model_runner.rs`、`fusion/panel.rs` | 原分析未见并发上限，token / 成本在调用后检查，提示超预算风险；实际生效约束待查 |
| `ProbeScheduler` | `winwincode-worker/src/probe_scheduler.rs` | 有持久意图 / 回执日志、分波执行、持久化取消；原报告称尚未接入生产，可作参考而非现成生产保证 |
| MCP 并行 | `cx/core/src/tools/handlers/mcp.rs` | 原报告称并行依据服务器自报 `readOnlyHint`；须核实与 §11.3 受信默认值和目录声明的关系 |
| `spawn_agent` 系列 | `Collab` / `CoreControl` 路径 | 原报告称 Collab 默认开启、CoreControl 不做能力检查、子 agent 不扣任务预算、`Op::Interrupt` 不级联、崩溃后 spawn 记 `failed` 可能触发重复 spawn；均须追踪生产路径验证 |
| `submit_change_batch` | `change_batch_identity.rs`、Worker 持久进度与 `delegated_batch_history` | 内容派生身份，含 `state_uncertain`，使用 `RepairLoopBudget`，可从持久历史恢复；原分析认为最接近设计要求，不等于完全满足设计 |
| 子线程租约谱系 | `SharedAuthoritySource`、`fence_binding` | 子线程沿用根租约，围栏可阻止谱系继续写入；不能据此断言执行 / 计费已经停止 |
| StrongFlow 阶段委托 | Delivery 的 WorkRun、`StrongFlowRoleBudget` | 原报告只找到预算声明，未找到 Rust 执行点；需重新验证 |
| Worker→Worker、Device→Device 交接 | 原分析未发现实现 | 代码中名为 handoff 的部分是终态回执、上下文压缩记录等；不据名称推断已有跨节点委托 |

需要分别核实：同一派发入口不等于共享并发控制；已有账本不等于模型 / HTTP / 工具身份和恢复统一；已有子 agent 不等于预算、取消与恢复随父任务一起管理。

### A.3 可复用机制与待验证缺口

可优先评估复用：

- `tool_requests` / `tool_attempts` 的 New / Replay / IdentityConflict 与先 claim 再执行。
- `execution='uncertain'` + `effects:"unknown"` 作为 `outcome_unknown` 的载体，`reconcile_execution` 作为 §10.3 核对入口。
- `winwincode-network` 的 Replay / Acceptance 及 GET / HEAD 区分，作为 §10.2 HTTP 默认分类的基础。
- `FuturesOrdered` 的并发执行 / 有序提交、`ProbeScheduler` 意图 / 回执日志、`RepairLoopBudget` 的“用量未知则预算关闭”保护。
- 租约谱系和 `fence_binding` 作为父授权继承及状态写入围栏；另补实际中断与停止确认。

待在目标提交证明或排除的缺口：

- 不变量 16：原分析提出缺门放行、CoreControl 能力检查缺口、模型 / HTTP / 工具账本独立等判断。`codex_delegate`（guardian 评审、`/review` 子会话）看起来未装门，**仅为静态判断，生产可达性尚未验证，不能写成已证实绕过**。
- 不变量 10：原报告指出崩溃调用统一记 `failed`、缺少 `outcome_unknown`、未见按“工具 + 参数摘要”拦截同参新调用。
- 调用身份：原报告中直接调用使用模型 `call_id` 摘要，Code Mode 使用 `(scope, cell, tool-N)` 摘要，模型调用采用调用方 `request_id`，需对照 §11.1 的网关分配要求。
- 预算继承、并发上限、取消级联及被中止 R1 留在 pending 等判断，必须核实当前生产装配与既有约束后再立缺陷。
- MCP 注解作为并行依据的实际边界，应对照 §11.3 核实，不能把注解本身当信任依据。

### A.4 F4a / F4b / F4c 提议拆分

原评论建议把 F4 分为三个增量；本稿保留这些建议标签，不声称已有获批 PR、工期或完整外部任务清单。

**F4a：账本与调用边界**

- 网关分配 `call_id`；能力目录声明重试类别；将 `outcome_unknown` 作为正式状态；按排障需要形成统一调用日志视图。
- 调用边界缺失时失败即关闭；核实 `codex_delegate` 的实际装配与生产可达性后，为可达子会话补足相同的身份、权限与网关约束。
- 验证相同逻辑调用的恢复与同参新调用拦截，遵守 §10.3、§10.4、§11.1。

**F4b：批量与并发**

- 每个批成员独立 `call_id`；为需要的恢复与诊断增加 `batch_id` 和序号。
- 直接工具调用与 Code Mode 共用并发限流；模型批预算派发前预留。
- 批取消时，已发出而结果不明的 R1 记为 `outcome_unknown`，不留无解释 pending；批内崩溃按成员的重试类别恢复。
- 继续保留“并发执行、按顺序提交”的有用行为，不以批级状态掩盖成员结果未知。

**F4c：委托与父子生命周期**

- 提议新增固定 R1 的 `delegate` 调用类型；`spawn_agent` 经过网关；若尚不能满足约束，则 kernel 显式关闭 Collab，而非默认绕过。
- 派发时从父预算预留给子，子终态时结算，不能令同一份预算在父子两端同时可用。
- 取消时既围栏状态写入，又中断子线程子树及在途请求；分别证明迟到结果被拒绝和执行确实停止。外部操作无法撤回时如实保留未知结果，不能用“已取消”掩盖副作用。
- 委托结果未知时记 `outcome_unknown`，不重新 spawn；不得因为未知就直接回收预算，也不能永久挂账。应提供以下待实现的核对、结算与解除规则。
- Worker 间交接列为本轮提议的非目标；不改变 §19 中需另行评审的后续扩展范围。

**未知委托预算的恢复边界（以后续评论修正为准）**

| 核对状态 | 处理要求 |
|---|---|
| 确认已发生且可获得结果 / 用量 | 记录核对证据，补记结果并按确认用量结算；解除不再需要的预留 |
| 确认未发生且不存在继续执行 / 计费风险 | 记录证据，解除对应预留；如需重试，按原逻辑调用及 §10.3 恢复，不能直接当新委托重新 spawn |
| 仍未知，或无法确认成本 / 执行已停止 | 保持保守预算保护，不恢复这部分可用额度、不重新 spawn；继续可用的核对途径，无法核对则按 §10.3 阻塞并记录用户决定。具体证据标准、结算与解除流程待实施设计明确，不能永久无处理入口地占用预算 |

上述是后续意见要求的恢复规则整理，不冒充现有代码能力；不能把“用户决定”解释为无需预算保护。普通模型调用仍遵守 §10.3 的独立规则：无法查询或幂等时将成本上限计入已用预算，剩余足够才允许重新调用并记录 `possible_duplicate_charge`，不足则阻塞。该例外不自动授权重新 spawn 委托。

### A.5 验收标签对应说明

首条评论提出向 A4、A6、A7、A8、A10 追加批量 / 委托验收，但原正文 §17 未为条目正式编号。本稿保留这些来源标签，不虚构它们所属的独立验收表；实施时应核对实际任务清单。若按原 §17 列表顺序理解，分别关联未知结果重复拦截、无效调用覆盖、Code Mode 身份恢复、统一调用日志、MCP R1 崩溃；新增可执行检查集中在 §17.2。

---

## 附录 B. 实施前确认与分组检查清单

以下是落实本稿所需的待办，不构成估时、排期或已接受承诺；所有检查项均待验收，本稿未将任何项标记为已完成。

### B.1 基线与证据

- [ ] 固定目标提交，复核附录 A 的生产入口、能力检查、并发限流、父子预算与恢复路径；记录哪些旧静态判断成立、失效或仍未证实。
- [ ] 对 `codex_delegate` 单独给出生产可达性与实际网关装配证据；在验证前保留“待核实”。
- [ ] 保留源评论链接及测试对应代码版本；不将文档审阅替代运行验证。

### B.2 远程部署与绑定（§4、§9.1、§13）

- [ ] 明确监听地址 / 对外 URL 的配置入口、优先级和 Device 持久化方式。
- [ ] 明确 TLS 终止点、可信代理范围、后端访问限制与远程认证；准备直接 TLS 和代理 TLS 两种部署验收。
- [ ] 实现稳定绑定请求 ID、设备密钥预持久化、令牌消费 / 绑定 / 幂等结果原子事务及私钥持有证明。
- [ ] 明确恢复记录保留范围、期限、过期处理与吊销不可复活规则。

### B.3 生命周期与恢复（§8、§9.7—9.10、§10）

- [ ] 明确重连退避、永久认证失败分类，以及离线租约到期 / 宽限期组成的最坏停止时间上限；以实际配置验收，不把建议默认值当实测保证。
- [ ] 实现吊销已有连接与租约、在线停止、离线限时停止，分别收集围栏与进程退出证据。
- [ ] 保持历史事实与当前授权分离：旧 epoch 日志补传不推进状态，应用恢复核对仍在当前恢复租约下进行。
- [ ] 保留审批绑定、基线变化不自动合并 / 变基、重新验证审批，以及 Device 验证 / Server 不重跑验证的边界。

### B.4 网关、批量与委托（§10、§11、附录 A）

- [ ] 优先消除经证实的重复执行、超预算、取消不彻底路径，再按 F4a / F4b / F4c 增量补齐。
- [ ] 明确委托未知结果的核对证据、预算结算 / 解除条件与人工处理入口；禁止无依据预算释放、直接重新 spawn 或永久悬挂。
- [ ] 保留任务级配置快照、调用级覆盖校验、MCP 共享隔离与宿主重启不盲目重放。

### B.5 运行验收（§17）

- [ ] 执行原有全部端到端故障注入项、§17.1 两机远程场景及 §17.2 批量 / 委托场景。
- [ ] 每项记录代码版本、配置、输入、预期 / 实际结果、两端日志、绑定记录、任务结果与必要进程退出证据。
- [ ] 未通过项、不可复现项、未执行项分别标明；通过局部验收不能表述为整个架构已完成或无缺陷。
