import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')

test('embedded Codex completes a streamed DSH turn with a real tool call', () => {
  const child = spawnSync(process.execPath, [
    resolve(root, 'tests/fixtures/native-dsh-model-turn.mjs'),
  ], {
    cwd: root,
    encoding: 'utf8',
    timeout: 20_000,
  })

  assert.equal(child.signal, null, `native child terminated by ${child.signal ?? 'no signal'}`)
  assert.equal(child.status, 0, child.stderr || child.stdout)
  const report = JSON.parse(child.stdout.trim().split('\n').at(-1))
  assert.equal(report.submission.status, 'started')
  assert.equal(report.calls.length, 2)
  assert.deepEqual(
    report.calls.map(call => [call.provider, call.model]),
    [
      ['deepseek-compatible', 'fixture-coder'],
      ['deepseek-compatible', 'fixture-coder'],
    ],
  )
  assert.ok(report.calls[0].toolNames.includes('exec_command'))
  assert.equal(report.calls[0].hasToolResult, false)
  assert.equal(report.calls[1].hasToolResult, true)
  assert.ok(report.calls.every(call => call.signalMatches))
  assert.ok(report.eventKinds.includes('exec_command_begin'))
  assert.ok(report.eventKinds.includes('exec_command_end'))
  assert.ok(report.eventKinds.includes('agent_reasoning'))
  assert.ok(report.eventKinds.includes('agent_message_content_delta'))
  assert.ok(report.eventKinds.includes('turn_complete'))
  assert.ok(report.normalizedKinds.includes('turn.started'))
  assert.ok(report.normalizedKinds.includes('tool.started'))
  assert.ok(report.normalizedKinds.includes('tool.completed'))
  assert.ok(report.normalizedKinds.includes('reasoning.delta'))
  assert.ok(report.normalizedKinds.includes('message.completed'))
  assert.ok(report.normalizedKinds.includes('usage.updated'))
  assert.ok(report.normalizedKinds.includes('approval.requested'))
  assert.ok(report.dshSessionAppendTypes.includes('turn/start'))
  assert.ok(report.dshSessionAppendTypes.includes('tool/call'))
  assert.ok(report.dshSessionAppendTypes.includes('tool/result'))
  assert.ok(report.dshSessionAppendTypes.includes('assistant/message'))
  assert.ok(report.dshSessionAppendTypes.includes('turn/end'))
  assert.equal(report.dshSessionAppendTypes.filter(type => type === 'user/message').length, 1)
  assert.equal(report.dshSessionAppendTypes.filter(type => type === 'tool/call').length, 1)
  assert.equal(report.dshSessionAppendTypes.filter(type => type === 'tool/result').length, 1)
  assert.equal(report.dshSessionAppendTypes.filter(type => type === 'assistant/message').length, 1)
  assert.equal(report.projectionReplayMatches, true)
  assert.equal(report.sessionReplayMatches, true)
  assert.equal(report.sourceIdentitiesComplete, true)
  assert.equal(report.approvalResponses.length, 1)
  assert.match(report.approvalResponses[0].approvalId, /.+/u)
  assert.match(report.approvalResponses[0].submissionId, /^[0-9a-f-]{36}$/u)
  assert.ok(report.agentMessages.includes('model port complete'))
  assert.deepEqual(report.errors, [])
  assert.equal(report.credentialPresent, false)
})
