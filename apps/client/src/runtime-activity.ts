// SPDX-License-Identifier: Apache-2.0
import type { RuntimeActivityProjection } from './generated/contracts.js'
import { redactPublicText } from './public-redaction.js'

const STATUS = {
  running: '运行中', completed: '已完成', failed: '失败', declined: '已拒绝',
  cancelled: '已取消', unknown: '状态待核对',
} as const
const DIAGNOSIS = {
  repeated_operation: '疑似重复调用', alternating_cycle: '疑似交替循环',
  branch_expansion: '疑似执行分支扩张', wait_cycle: '疑似等待成环',
  unavailable_wait: '等待对象已失效',
} as const

/** Both product surfaces render the same accepted Core relationships. */
export function runtimeActivityPresentation(activity: RuntimeActivityProjection) {
  const meta = activity.coreTool
  const call = meta?.call
  const relations: string[] = []
  const details: string[] = []
  if (call?.parentRequestSequence) relations.push(`父请求 #${String(call.parentRequestSequence)}`)
  else if (call?.parentCallId) relations.push(`父调用 ${call.parentCallId}`)
  if (call?.cellId) relations.push(`代码运行 ${call.cellId}`)
  if (meta?.cell) relations.push(`父请求 #${String(meta.cell.parentRequestSequence)}`)
  if (meta?.wait) relations.push(`等待代码运行 #${String(meta.wait.targetCellSequence)}`)
  if (meta?.sharing) relations.push(`${meta.sharing.kind === 'reuse' ? '复用' : '合并等待'}请求 #${String(meta.sharing.sourceRequestSequence)}`)
  if (call?.execution) details.push({ running: '原执行已登记', completed: '执行已完成', uncertain: '执行结果待核对' }[call.execution])
  if (call?.disposition && call.disposition !== 'pending') details.push(call.disposition === 'accepted' ? '结果已接受' : '结果已拒绝')
  if (call?.delivery === 'offered') details.push('结果已交付给调用者')
  if (call?.inputValidation) details.push({ verified: '输入来源已验证', mismatch: '输入来源不匹配', unknown: '输入来源待核对' }[call.inputValidation])
  if (meta?.recovery) details.push({ unavailable: '恢复回执不可用', unconfirmed: '恢复状态待核对', running: '下游回执：运行中', exited: '下游回执：已退出' }[meta.recovery.state])
  const diagnosis = meta?.diagnosis
  const title = diagnosis
    ? diagnosis.kind === null ? '模型已回应运行诊断' : DIAGNOSIS[diagnosis.kind]
    : call?.toolName ?? activity.command ?? ({ command: '命令', test: '测试', tool: '工具' }[activity.activityType])
  return Object.freeze({
    title: redactPublicText(title),
    status: diagnosis === undefined || diagnosis === null
      ? STATUS[activity.status]
      : diagnosis.kind === null ? '回应已记录'
        : diagnosis.delivery === 'offered' ? '已提示模型' : '等待提示模型',
    relations: Object.freeze(relations.map(redactPublicText)),
    details: Object.freeze(details),
    question: diagnosis?.question === null || diagnosis?.question === undefined ? null : redactPublicText(diagnosis.question),
    evidence: Object.freeze(diagnosis?.evidence.map(reference => `${reference.toolName} · 调用 ${reference.logicalId}`) ?? []),
  })
}

export function renderRuntimeActivity(document: Document, activity: RuntimeActivityProjection): HTMLElement {
  const view = runtimeActivityPresentation(activity)
  const row = document.createElement('div')
  row.className = 'wwc-runtime-activity'
  row.dataset.runtimeCallId = activity.callId
  const title = document.createElement('span')
  title.className = 'wwc-runtime-activity-title'
  title.textContent = `${view.title} · ${view.status}`
  row.append(title)
  for (const text of [...view.relations, ...view.details]) {
    const detail = document.createElement('span')
    detail.className = 'wwc-runtime-activity-detail'
    detail.textContent = text
    row.append(detail)
  }
  if (view.question !== null) {
    const question = document.createElement('p')
    question.className = 'wwc-runtime-activity-question'
    question.textContent = view.question
    row.append(question)
  }
  if (view.evidence.length > 0) {
    const evidence = document.createElement('ul')
    evidence.className = 'wwc-runtime-activity-evidence'
    for (const text of view.evidence) {
      const reference = document.createElement('li')
      reference.textContent = redactPublicText(text)
      evidence.append(reference)
    }
    row.append(evidence)
  }
  return row
}
