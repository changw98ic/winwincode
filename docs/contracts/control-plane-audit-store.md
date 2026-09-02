# Control Plane Audit Event 与不可变审计存储

- 正式决定：[ADR-0028](../decisions/0028-control-plane-worker-migration.md)
- 机器规则：[control-plane-audit-store.rules.json](./control-plane-audit-store.rules.json)
- 对应任务：`winwincode-9c4.16.3.4`
- 状态：Audit Event 和本地 SQLite 存储已实现，并由 Rust 黑盒测试执行

## 唯一写入路径

审计只走下面一条路径：

```text
已认证 actor + Policy 已允许的精确 scope
  → closed typed AuditEvent
  → canonical JSON + payload SHA-256
  → organization 内连续 sequence + previous digest
  → SQLite immutable event header + retained payload
  → scope-filtered read 或 immutable retention tombstone
```

`AuditEvent` 只接收结构化字段：actor、organization/workspace/project/repository、requestId、
稳定 action/result code、状态变化前后摘要、本地组件或 source IP，以及可选的 Delivery、
ProductSession、Lease、Publication 引用。`AuditEventId` 固定为 `aud_` 加 26 位 Crockford
字符。actor 只能是 User、ServiceAccount 或 System；scope 只能停在 organization、workspace、
project 或 repository 中的一个精确层级。

action 只有 command、approval、policy、worker lease、model invocation、Delivery state 和
Publication 七类。成功的状态变化必须给出不同的 before/after SHA-256；拒绝和失败只能记录
unchanged 状态。这样，失败或明确拒绝也进入同一条有序审计链，但不会伪装成业务状态已经改变。

同一个 `AuditEventId` 和完全相同内容再次写入时，返回第一次的 sequence 和 digest，不追加
第二条记录。相同 ID 改动任何事件内容时返回 `RequestConflict`。

## 连续顺序和损坏检查

`AuditStore` 使用单独的 `audit.sqlite3`，启用 WAL 和 FULL 同步。每个 organization 有独立、
从 1 开始且无缺口的 sequence。每个 event header 的 SHA-256 同时绑定：

- organization 和 sequence；
- 前一条 event digest；
- eventId、发生时间和完整 scope；
- retention 类型与期限；
- canonical event payload digest。

event header 和 retention tombstone 都由 SQLite trigger 禁止 UPDATE/DELETE。读取或完整校验会
重新计算 payload digest、header digest、前后链接和 chain head，并核对 payload 中的 eventId、
scope、时间和 retention 是否仍与 header 一致。payload、header、sequence、tombstone 或 chain
head 缺失或变化时，读取按 `Corrupt` 停止，不返回未经证明的审计事实。

多个连接同时写不同事件时，SQLite 写事务把它们排成一条连续链；多个连接同时重放完全相同
事件时，只产生一个 sequence。

## 权限过滤

Policy 层先完成认证和授权，再把允许读取的精确 `AuditAccess` 交给存储。`AuditAccess` 本身不是
登录凭据，也不负责决定用户是否有权限。存储只负责按 organization、workspace、project、
repository 的完整层级过滤，并把单页上限固定为 200 条。

organization 级读取能看到该 organization 的所有层级；workspace 级只能看到该 workspace
及其 project/repository；project 级只能看到该 project 及其 repository；repository 级只返回
该 repository。另一个 organization 的 payload 不会进入结果。

## 保留期限

retention 只有 `until-millis` 和 `indefinite` 两种。有限期限到达前不能删除 payload；
`indefinite` payload 不参与清理。期限到达后，清理事务先写入不可变 tombstone，再只删除
canonical payload bytes。event header、payload digest、sequence、previous digest 和 tombstone
继续保留，因此 organization 的顺序仍可验证。

payload 清理后，完全相同的 eventId 重放仍返回原 sequence 和无 payload 的原 header，不会把
已过期内容写回数据库。直接删除 payload 而没有匹配的 tombstone 会被判为损坏。

## 敏感信息边界

事件没有任意 payload、日志、错误正文、HTTP header、prompt、response 或 credential 字段。
action、result、本地组件、provider 和 model 只接受最长 128 字节的 portable token。模型调用
只保存 provider/model 身份、输入/输出 SHA-256 和 token 数量；原始 prompt、原始回答、请求体、
授权信息和远端诊断没有可写入的字段。即使调用方先用 JSON 反序列化构造事件，`append` 仍会
重新执行完整校验，不能绕过这些限制。

## 当前集成边界

阶段 3.4 交付的是唯一 typed `AuditEvent` 和 `AuditStore`。当前 canonical HTTP schema 没有
Audit query，本任务也不增加第二套 HTTP 或产品 command API。Policy 授权、Publication 调用和
整条 Control Plane 产品写入接线由 `winwincode-9c4.16.3.5`、`.3.6`、`.3.7` 依次完成；这些
后续任务必须在每次成功状态变化、拒绝和失败时调用本 seam，并由最终切换门禁证明审计完整。

## 自动验证

机器门禁实际执行 `winwincode-audit` 的 Rust 黑盒测试，而不是只查测试名。测试覆盖：

1. 成功状态变化、拒绝、失败和七类 action；
2. 重启、并发不同事件的连续顺序、并发完全相同事件的单次写入和 changed ID 冲突；
3. organization/workspace/project/repository 四级过滤和分页；
4. 到期清理、不可变 tombstone、清理后重放和未授权 payload 删除；
5. payload/header/head 损坏后停止读取；
6. 模型摘要的 closed 字段，以及反序列化后再次校验敏感文本。
