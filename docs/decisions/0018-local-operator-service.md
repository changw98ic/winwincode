# ADR-0018：一个本地作业服务同时供 DSH Remote 和 CLI 使用

- 状态：已接受
- 日期：2026-08-22
- 对应任务：`winwincode-9c4.9.2`
- 接口：[`packages/contracts/src/strongflow-operator.ts`](../../packages/contracts/src/strongflow-operator.ts)
- 实现：[`packages/strongflow/src/operator-service.ts`](../../packages/strongflow/src/operator-service.ts)
- 测试：[`tests/strongflow-operator-service.test.mjs`](../../tests/strongflow-operator-service.test.mjs)

## 结论

DSH 高级工作台和 `winwincode` CLI 不各自实现作业状态。两者都把严格的
`StrongFlowOperatorRequest` 交给同一个 `StrongFlowLocalJobService`，并读取同一个
`StrongFlowOperatorResponse`。服务使用现有的追加式作业记录和内容寻址制品存储，
所以刷新界面、CLI 退出或 Host 重启后，作业身份、定义、人工决定、事件序号和制品
链接都从本地正式记录恢复，而不是从某个界面的内存恢复。

服务只在六个明确的变更操作中改写状态：创建、三种人工决定、取消和恢复。浏览器
断开、CLI 收到 `SIGINT` / `SIGTERM`，或一次 `follow` 等待被中止，都只停止当前调用；
它们不会暗中生成取消事件。

## 本地记录

服务在 WinWinCode home 下使用三个自己的小型记录目录：

- `strongflow-operator/jobs/`：创建作业时需要的仓库位置、基础版本、标题和用户请求
  制品身份。用户请求正文只进入 `USER_REQUEST` 制品，不在这里复制；
- `strongflow-operator/request-claims/`：先占用一个变更请求身份，防止同一
  `requestId` 被换成另一项操作；
- `strongflow-operator/requests/`：保存第一次已确定的结果，使进程重启后的相同重试
  返回完全相同的响应。

文件先完整写入、同步，再用独占链接发布。文件名使用请求或作业身份的 SHA-256，
认证证明只参与完整请求摘要，不以明文写入这些记录。作业事件仍由
`StrongFlowJobStore` 保存，制品正文和元数据仍由 `StrongFlowArtifactStore` 保存，
没有第二套作业数据库。

创建、审核、取消和恢复还使用由 `requestId` 确定生成的作业、审核或控制来源身份。
如果进程在状态已经写入、结果回执尚未写入时退出，同一请求可以从正式事件中恢复，
而不会再生成第二个决定或控制事件。相同 `requestId` 配不同内容返回
`JOB_CONFLICT`。

## 人工审核

人工审核仍由 `StrongFlowHumanReviewGate` 完成。服务在写决定前同时核对：

1. 作业正在等待人工审核；
2. 请求中的需求、方案、系统架构图和流程图四个身份全部等于当前版本；
3. `local-ui` 使用 `local-session` 证明，`cli` 使用 `local-peer` 证明；
4. 认证器返回一个有效的本地人工身份。

通过后，审核事件先进入追加式作业记录，随后同一份 `HUMAN_REVIEW_RECORD` 进入制品
存储。操作响应中的作业视图、事件和审核正文指向同一个决定。过期页面返回
`STALE_DEFINITION` 和当前四个身份；认证证明不会进入成功结果、错误、事件或导出。

打包 Host 可以通过配置或 `WINWINCODE_UI_AUTH_PROOF`、
`WINWINCODE_CLI_AUTH_PROOF` 注入本地证明。没有配置对应证明时，读取操作继续可用，
但该通道的人工决定会明确返回认证错误，不会自动放行。

## DSH Remote

Host 侧的 `StrongFlowOperatorRemoteService` 继承 DSH 的
`TypertRemoteService`，只公开 `strongflow/invoke`。最后一个参数名固定为 `signal`，
因此 DSH Gateway 会把连接中止信号注入当前调用。Remote 在调用服务前再次解析不可信
请求；格式错误时返回公开的 `INVALID_REQUEST` 信封，不把异常或输入正文交给客户端。

Client 侧只在 StrongFlow 高级界面加载时挂载一份严格 Typert 描述。请求和结果分别
使用共享接口的解析器校验。DSH 原始聊天界面仍是默认入口，不需要挂载第二套前端或
第二个 Agent 循环。

## CLI

`apps/host/src/strongflow-cli.ts` 从共享的 13 个命令描述构造同样的请求。它不读取或
改写作业文件，也不复制审核逻辑。JSON 成功和错误信封与 DSH Remote 相同；
`follow` 每次输出一行完整响应，其中包含下一次连接使用的准确游标。

CLI 信号处理只中止本次 `invoke`，并按接口返回 130 或 143。持久作业只能通过
`winwincode cancel` 取消。安装包级、真实进程和信号 smoke 由后续
`winwincode-9c4.9.3` 单独覆盖；本决策的测试覆盖两个适配器使用相同身份、定义读取、
旧版本审核、三种人工决定、请求重试、游标续读、中止、取消、恢复和重启。

## 执行唤醒边界

服务可以接收一个 `StrongFlowOperatorJobScheduler`。创建、批准、要求修改和恢复只把
已经写入正式记录的作业交给它；操作响应不等待角色执行完成。服务启动后会扫描未结束、
未等待人工、未中断且没有活动阶段的作业并重新通知调度器。这样 Host 重启可以继续
流程。显式取消写入正式取消事件后，服务还会通知调度器停止当前工作；普通 Remote 或
CLI 断开不会触发这条通知，也不会取得作业所有权。

## 版本

这是本地服务和两个适配器的第一个实现，没有旧服务、旧命令或旧存储格式需要保留。
以后改变公开格式时，迁移到一个新的正式版本并删除旧路径，不增加双读、双写或旧命令
别名。
