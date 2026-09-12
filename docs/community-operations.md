# Community 安装、使用与排障

本文是 Community 当前单一路径的操作入口。组件边界和数据模型见[架构](architecture.md)，完整 Server 配置见[公网部署](deploy-public-server.md)，发布产物验收见[发布门禁](release-gate.md)。

## 安装与启动

源码运行需要 Node.js 24、Corepack/pnpm 11.7.0 和 Rust 1.95.0：

```bash
corepack pnpm install --frozen-lockfile
corepack pnpm build
corepack pnpm start
```

容器部署使用仓库根的 `compose.yaml`。按 [`deploy/README.md`](../deploy/README.md) 配置 TLS、一次性 Owner proof、远程 Worker 凭据、仓库身份和发布身份；Compose 只运行 Web 与 Backend，Device Client 始终运行在代码机器上。

Device Client 是供 `launchd` 或 `systemd --user` 守护的前台进程；Server URL 必须是受信任证书的 HTTPS origin（也可直接给出精确 exchange URL）：

```bash
export WWC_DEVICE_DATA_DIRECTORY=/path/to/device-data
export WWC_DEVICE_SERVER_URL=https://server.example:8443
wwc device serve
```

服务管理器负责进程启动和退出，`wwc` 负责产品内控制：

```bash
wwc device status
wwc device restart
wwc device logs
```

`restart` 只重建 daemon session 并生成新的 `clientInstanceId`，保留 SQLite、outbox、occupancy 与 Worker registry；`logs` 最多返回最后 200 行脱敏服务日志。也可在每条命令显式传 `--data-dir PATH`，覆盖环境变量。

四目标 Device 安装包的 `service/` 目录带对应的用户级守护定义。macOS 把 plist 中三个 `__WWC_*__` 占位符替换为绝对路径和 HTTPS URL 后，安装为 `~/Library/LaunchAgents/dev.winwincode.device-client.plist`，再运行：

```bash
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/dev.winwincode.device-client.plist
launchctl kickstart -k "gui/$(id -u)/dev.winwincode.device-client"
```

Linux 把 service 中的 `__WWC_BINARY__` 替换为绝对路径，在 `~/.config/winwincode/device-client.env` 写入 `WWC_DEVICE_DATA_DIRECTORY` 和 `WWC_DEVICE_SERVER_URL`，然后安装到 `~/.config/systemd/user/winwincode-device-client.service`：

```bash
systemctl --user daemon-reload
systemctl --user enable --now winwincode-device-client.service
```

卸载前先分别执行 `launchctl bootout "gui/$(id -u)/dev.winwincode.device-client"` 或 `systemctl --user disable --now winwincode-device-client.service`；删除守护定义不删除 Device 数据目录。

## 首次使用

1. 启动 Server，在登录页用 `WWC_SERVER_BOOTSTRAP_PROOF` 创建唯一 Owner；初始化成功后不要继续分发该 proof。
2. 在代码机器启动 Device Client，浏览器的 Client 管理页输入本机一次性连接码完成 enrollment。CLI 的 `wwc device status --data-dir PATH` 只显示无秘密状态；`refresh-code` 才会显示一次明文连接码。管理 CLI 不会改写运行中 daemon 的实例身份。
3. 用 `wwc repo add PATH --data-dir PATH` 注册代码仓库；非 Git 目录只有显式加 `--init` 才会初始化。
4. 在设置页建立 Provider 与 Credential reference，并执行连接测试。失败时保持未就绪，不会把缺失凭据记成可用。
5. 回到 Chat 提交第一项任务；进入 StrongFlow 后检查 Spec、候选、逐条证据和 Verifier 结论，再决定交付。

## 规则与验证信任

仓库规则、个人规则和当前指令的优先级与删除语义见 [ADR-0034](decisions/0034-community-knowledge-lifecycle.md)。新增机器规则时修改对应的 `docs/contracts/*.rules.json` 和唯一消费它的测试；不要在 UI、Worker 和 Control Plane 各复制一份规则。

Reviewer 与 Verifier 只能读取已冻结候选，使用彼此独立的 Session/Worker 身份。证据必须指向实际运行事件；写入候选、缺少必需 Evidence、身份冲突或结果不一致都会使 Verdict 失败或不确定。详细规则见[证据、Verdict 与返工合同](contracts/delivery-evidence-verdict-rework.md)。

协作只保存责任、Presence、决定和外部引用。外部讨论仍留在 GitHub、Jira、Linear、Slack 或 Teams；任何协作者的完成消息都不能替代当前候选的 Verifier 证据和 Owner 决定。

## 恢复、备份与升级

Server、Worker 或 Device Client 重启后使用持久化 cursor、receipt、Lease 和 fencing token 恢复；不要删除数据目录来“重试”。状态不明时先运行 `wwc doctor` 和 `wwc device status`，再检查 `/health`。

一致性备份与恢复使用：

```bash
wwc backup snapshot --store server --data-dir PATH --output BACKUP
wwc backup snapshot --store device --data-dir PATH --output BACKUP
wwc backup verify --from BACKUP
wwc backup restore --store server --data-dir PATH --from BACKUP
```

恢复前停止对应进程。Device 备份不含明文设备凭据，只能在持有原凭据的同一设备回绑；设备丢失时重新 enrollment。升级只使用一个完整、已验证的发布版本；回滚选择上一份完整产物和与其匹配的备份，不混用协议、二进制或静态文件。

## 安全检查

- 公网只暴露 Server TLS endpoint；浏览器只从 `runtime-config.js` 读取一个 HTTPS `serverUrl`。
- TLS 私钥、Worker credential 和 Provider secret 不进入 Git、日志、备份或诊断输出。
- Publication 在外部写入前必须有当前候选、当前 Verdict 和有效人工批准。
- 发布前运行 `corepack pnpm verify`、四目标 artifact 校验和秘密/路径扫描。

## 常见问题

| 现象 | 检查与处理 |
| --- | --- |
| `/health` 非 200 | 检查 Server 日志、TLS 文件、数据目录权限和全部必填环境变量；健康失败时不要继续发布。 |
| 浏览器无法连接 | 检查 `runtime-config.js` 的 HTTPS origin、Server `WWC_SERVER_ALLOWED_ORIGINS` 和证书信任链。 |
| Device 显示离线 | 检查出站网络、Server remote Worker 模式、凭据有效期和本地 Device 数据目录；保留目录后重启服务以重放 outbox。 |
| 仓库不可用或 HEAD 改变 | 运行 `wwc repo list`，确认原路径、Git common directory 和冻结 revision；重新绑定前不要执行旧任务。 |
| Provider 测试失败 | 核对 endpoint、模型能力、Credential reference 和秘密文件权限；未知费用保持未知，不写成零。 |
| 验证失败或不确定 | 从 StrongFlow 打开对应 criterion、Evidence 和 finding；修复后生成新候选并重新验证，不改写旧结论。 |
| SQLite/WAL 异常 | 先运行 `wwc backup repair --store server|device --data-dir PATH` 只读诊断；只有允许的 WAL checkpoint/临时文件清理才加 `--apply`，其余情况从已验证备份恢复。 |
| 需要提交诊断 | 只提供脱敏后的稳定错误码、版本、commit、目标、时间和 Evidence 摘要；不要粘贴凭据、绝对路径、完整模型正文或数据库。 |
