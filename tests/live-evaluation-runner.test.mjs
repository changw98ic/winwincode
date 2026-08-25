import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import { once } from 'node:events'
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { createServer } from 'node:http'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'
import { setTimeout as delay } from 'node:timers/promises'

import {
  LiveEvaluationError,
  disposeCompletedRoleSession,
  parseLiveEvaluationConfig,
  runLiveEvaluation,
} from '../scripts/live-evaluation.mjs'
import { measureLiveEvaluationResult } from '../scripts/evaluation-measures.mjs'

const root = resolve(import.meta.dirname, '..')

function assertMeasureSources(value) {
  if (Array.isArray(value)) {
    value.forEach(assertMeasureSources)
    return
  }
  if (typeof value !== 'object' || value === null) return
  if (Object.hasOwn(value, 'value')) {
    assert.equal(Array.isArray(value.sourceRefs), true)
    assert.equal(value.sourceRefs.length > 0, true)
  }
  Object.values(value).forEach(assertMeasureSources)
}

function checked(command, arguments_, options = {}) {
  const result = spawnSync(command, arguments_, {
    cwd: options.cwd,
    encoding: 'utf8',
    env: options.env ?? process.env,
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0 || result.signal !== null) {
    throw new Error(`${command} ${arguments_.join(' ')} failed: ${result.stderr}`)
  }
  return result.stdout.trim()
}

async function waitForFile(path, timeoutMillis = 5_000) {
  const deadline = Date.now() + timeoutMillis
  while (Date.now() < deadline) {
    try {
      await readFile(path)
      return
    } catch (error) {
      if (error?.code !== 'ENOENT') throw error
    }
    await delay(20)
  }
  throw new Error(`timed out waiting for ${path}`)
}

async function fixtureRepository(rootDirectory) {
  const repository = join(rootDirectory, 'source')
  await mkdir(join(repository, 'src'), { recursive: true })
  await mkdir(join(repository, 'test'), { recursive: true })
  await writeFile(join(repository, '.gitignore'), 'executor-only.tmp\n')
  await writeFile(join(repository, 'src', 'value.mjs'), "export const value = 'before'\n")
  await writeFile(join(repository, 'test', 'value.test.mjs'), [
    "import assert from 'node:assert/strict'",
    "import test from 'node:test'",
    "import { value } from '../src/value.mjs'",
    '',
    "test('live candidate', () => assert.equal(value, 'after'))",
    '',
  ].join('\n'))
  await writeFile(join(repository, 'package.json'), `${JSON.stringify({
    name: 'winwincode-live-evaluation-fixture',
    private: true,
    type: 'module',
    scripts: { test: 'node --test' },
  }, null, 2)}\n`)
  checked('git', ['init', '--initial-branch=main'], { cwd: repository })
  checked('git', ['config', 'user.name', 'Evaluation Fixture'], { cwd: repository })
  checked('git', ['config', 'user.email', 'fixture@winwincode.invalid'], { cwd: repository })
  checked('git', ['add', '--all'], { cwd: repository })
  checked('git', ['commit', '-m', 'Create live evaluation fixture'], { cwd: repository })
  return Object.freeze({
    repository,
    commit: checked('git', ['rev-parse', 'HEAD'], { cwd: repository }),
  })
}

function configFor(repository, baseURL) {
  const deliveryId = 'dlv_1P8BR1KDNS6R1ENCA8F2KZM1TZ'
  return {
    schemaVersion: 1,
    runId: 'live-evaluation-fixture',
    projectionVersion: 2,
    repository: {
      sourcePath: repository.repository,
      expectedCommit: repository.commit,
    },
    provider: {
      route: 'deepseek',
      model: 'deepseek-v4-flash',
      apiKeyEnv: 'WINWINCODE_EVALUATION_TEST_API_KEY',
      baseURL,
      reasoningEffort: null,
      timeoutMillis: 10_000,
    },
    budgets: {
      maxWallTimeMillis: 60_000,
      maxModelCalls: 20,
      maxTurns: 8,
      maxTokensPerCall: 1_024,
      maxTotalTokens: 100_000,
      maxCostUsdMicros: 1_000_000,
      pricing: {
        source: 'fixture pricing',
        inputUsdMicrosPerMillionTokens: 100,
        outputUsdMicrosPerMillionTokens: 200,
        cacheReadUsdMicrosPerMillionTokens: 10,
        cacheWriteUsdMicrosPerMillionTokens: 20,
      },
    },
    deliverySpec: {
      schemaVersion: 3,
      id: 'spec-live-evaluation-fixture',
      deliveryId,
      revision: 2,
      title: 'Live evaluation fixture',
      goal: 'Change the exported fixture value from before to after.',
      scope: ['src/value.mjs'],
      outOfScope: ['A second Agent scheduler'],
      constraints: ['Use the embedded Codex kernel through DSH'],
      acceptanceCriteria: [{
        schemaVersion: 3,
        id: 'criterion-live-evaluation-fixture',
        description: 'The fixture exports the value after.',
        verificationMethod: 'Run node --test in the candidate repository.',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: 3,
        kind: 'local-git',
        locator: repository.repository,
      },
      baseRevision: repository.commit,
      maxReworkAttempts: 1,
      createdAtMillis: 1,
    },
    solution: {
      id: 'solution-live-evaluation-fixture',
      summary: 'Change one module and verify the exact frozen candidate.',
      approach: ['Edit the module', 'Run its test', 'Freeze the committed candidate'],
      components: [{
        id: 'component-live-value',
        label: 'Value module',
        responsibility: 'Export the accepted value.',
        kind: 'component',
        trustBoundary: 'Fixture repository',
        unresolved: false,
        repositoryPathPrefixes: ['src'],
      }],
      connections: [{
        id: 'connection-codex-value',
        from: 'platform:codex-core',
        to: 'component-live-value',
        label: 'Implements the approved change',
      }],
    },
    humanDecisions: {
      planReview: {
        action: 'approve',
        comments: 'Approve the exact fixture plan and diagrams.',
        requestedChanges: [],
      },
      deliveryReview: {
        action: 'approve',
        resolution: 'Approve the exact passing candidate and evidence.',
      },
    },
    execution: { commitMessage: 'Implement live fixture candidate' },
  }
}

async function rejectingOpenAiServer(secret) {
  let requestCount = 0
  const server = createServer(async (request, response) => {
    requestCount += 1
    for await (const _chunk of request) {
      // Drain the request before returning the provider failure.
    }
    response.writeHead(500, { 'content-type': 'application/json' })
    response.end(JSON.stringify({ error: { message: `provider rejected ${secret}` } }))
  })
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  const address = server.address()
  return Object.freeze({
    server,
    requestCount: () => requestCount,
    baseURL: `http://127.0.0.1:${String(address.port)}/v1`,
  })
}

async function interruptingOpenAiServer(interrupt) {
  let interruptCount = 0
  let requestCount = 0
  const requestStarted = Promise.withResolvers()
  const server = createServer((request, response) => {
    requestCount += 1
    if (requestCount === 1) {
      requestStarted.resolve()
      interruptCount += 1
      interrupt()
    }
    request.resume()
    response.writeHead(503, { 'content-type': 'application/json' })
    response.end(JSON.stringify({ error: { message: 'interrupted fixture request' } }))
  })
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  const address = server.address()
  return Object.freeze({
    server,
    interruptCount: () => interruptCount,
    requestCount: () => requestCount,
    requestStarted: requestStarted.promise,
    baseURL: `http://127.0.0.1:${String(address.port)}/v1`,
  })
}

function textOfContent(content) {
  if (typeof content === 'string') return content
  if (!Array.isArray(content)) return ''
  return content.map(block => typeof block?.text === 'string' ? block.text : '').join('')
}

function latestUserText(body) {
  return [...body.messages].reverse()
    .find(message => message.role === 'user')
    ?.content ?? ''
}

function completionChunk(model, delta, finishReason = null) {
  return {
    id: 'chatcmpl-fixture',
    object: 'chat.completion.chunk',
    created: 1,
    model,
    choices: [{ index: 0, delta, finish_reason: finishReason }],
  }
}

function writeStream(response, model, answer) {
  response.writeHead(200, {
    'content-type': 'text/event-stream',
    'cache-control': 'no-cache',
  })
  if (answer.type === 'tool') {
    const calls = answer.calls ?? [{
      id: answer.id,
      name: answer.name,
      arguments: answer.arguments,
    }]
    response.write(`data: ${JSON.stringify(completionChunk(model, {
      role: 'assistant',
      reasoning_content: answer.reasoning ?? 'Use the requested tools and inspect their results.',
      tool_calls: calls.map((call, index) => ({
        index,
        id: call.id,
        type: 'function',
        function: { name: call.name, arguments: JSON.stringify(call.arguments) },
      })),
    }))}\n\n`)
    response.write(`data: ${JSON.stringify(completionChunk(model, {}, 'tool_calls'))}\n\n`)
  } else {
    response.write(`data: ${JSON.stringify(completionChunk(model, {
      role: 'assistant',
      content: answer.text,
    }))}\n\n`)
    response.write(`data: ${JSON.stringify(completionChunk(model, {}, 'stop'))}\n\n`)
  }
  response.write(`data: ${JSON.stringify({
    id: 'chatcmpl-fixture',
    object: 'chat.completion.chunk',
    created: 1,
    model,
    choices: [],
    usage: { prompt_tokens: 20, completion_tokens: 10, total_tokens: 30 },
  })}\n\n`)
  response.end('data: [DONE]\n\n')
}

function commandTool(body) {
  const tool = body.tools?.find(entry => (
    entry.type === 'function'
    && isRecord(entry.function?.parameters?.properties)
    && Object.hasOwn(entry.function.parameters.properties, 'cmd')
  ))
  assert.notEqual(tool, undefined, 'Codex request must expose its command tool through DSH')
  return tool.function.name
}

function writeStdinTool(body) {
  const tool = body.tools?.find(entry => (
    entry.type === 'function'
    && isRecord(entry.function?.parameters?.properties)
    && Object.hasOwn(entry.function.parameters.properties, 'session_id')
    && entry.function.name === 'write_stdin'
  ))
  assert.notEqual(tool, undefined, 'Codex request must expose write_stdin for yielded commands')
  return tool.function.name
}

function runningExecSessions(messages) {
  return messages.flatMap(message => {
    const match = /Process running with session ID (\d+)/u.exec(textOfContent(message.content))
    return match === null ? [] : [Number.parseInt(match[1], 10)]
  })
}

function assertBackgroundPollResults(assistant, messages) {
  const pollCallIds = new Set(assistant.tool_calls
    .filter(call => call.function.name === 'write_stdin')
    .map(call => call.id))
  for (const message of messages) {
    if (!pollCallIds.has(message.tool_call_id)) continue
    assert.match(
      textOfContent(message.content),
      /Process (?:running with session ID \d+|exited with code \d+)/u,
      'write_stdin must report the same process as running or terminal',
    )
  }
}

function isRecord(value) {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function verificationJson(prompt, role) {
  const marker = 'The following exact normalized evidence sources were observed in your earlier turn:\n'
  const start = prompt.indexOf(marker)
  const end = prompt.indexOf('\nEvery evidence_sources', start)
  assert.notEqual(start, -1)
  assert.notEqual(end, -1)
  const evidence = JSON.parse(prompt.slice(start + marker.length, end))
  assert.deepEqual(Object.keys(evidence[0]).sort(), ['citation', 'outcome'])
  assert.deepEqual(Object.keys(evidence[0].citation).sort(), ['event_id', 'type'])
  const contractMarker = 'Result contract: '
  const contractStart = prompt.indexOf(contractMarker) + contractMarker.length
  const contractEnd = prompt.indexOf('.\nThe following', contractStart)
  const contract = JSON.parse(prompt.slice(contractStart, contractEnd))
  assert.deepEqual(contract.requiredEvidenceSourceFields, ['type', 'event_id'])
  const source = evidence.find(entry => entry.outcome === 'succeeded') ?? evidence[0]
  return JSON.stringify({
    protocol: contract.protocol,
    delivery_spec_id: contract.deliverySpecId,
    delivery_spec_revision: contract.deliverySpecRevision,
    candidate_ref: contract.candidateRef,
    findings: contract.criterionIds.map((criterionId, index) => ({
      finding_id: `finding-${role}-${String(index + 1)}`,
      criterion_id: criterionId,
      verdict: 'pass',
      explanation: `${role} observed the passing fixture check.`,
      evidence_sources: [source.citation],
    })),
  })
}

async function fakeOpenAiServer(options = {}) {
  const requests = []
  let backgroundPolls = 0
  let parallelToolResultFollowUps = 0
  const verificationResultAttempts = { reviewer: 0, verifier: 0 }
  const server = createServer(async (request, response) => {
    if (request.method !== 'POST' || request.url !== '/v1/chat/completions') {
      response.writeHead(404).end()
      return
    }
    const chunks = []
    for await (const chunk of request) chunks.push(chunk)
    const body = JSON.parse(Buffer.concat(chunks).toString('utf8'))
    requests.push(body)
    const assistantIndex = body.messages.findLastIndex(message => (
      message.role === 'assistant' && Array.isArray(message.tool_calls)
    ))
    if (assistantIndex !== -1) {
      const assistant = body.messages[assistantIndex]
      const results = body.messages.slice(assistantIndex + 1)
        .filter(message => message.role === 'tool')
      assert.equal(Object.hasOwn(assistant, 'reasoning_content'), true)
      assert.deepEqual(
        results.map(message => message.tool_call_id).sort(),
        assistant.tool_calls.map(call => call.id).sort(),
      )
      assertBackgroundPollResults(assistant, results)
      if (assistant.tool_calls.length === 2) parallelToolResultFollowUps += 1
      const runningSessions = runningExecSessions(results)
      if (runningSessions.length > 0) {
        backgroundPolls += runningSessions.length
        writeStream(response, body.model, {
          type: 'tool',
          calls: runningSessions.map((sessionId, index) => ({
            id: `call-background-poll-${String(requests.length)}-${String(index + 1)}`,
            name: writeStdinTool(body),
            arguments: {
              session_id: sessionId,
              chars: '',
              yield_time_ms: 30_000,
            },
          })),
        })
        return
      }
    }
    const prompt = textOfContent(latestUserText(body))
    const system = textOfContent(body.messages.find(message => message.role === 'system')?.content)
      || body.messages.find(message => message.role === 'system')?.content
      || ''
    const role = system.includes('Independently verify')
      ? 'verifier'
      : system.includes('Independently review')
        ? 'reviewer'
        : system.includes('Implement only')
          ? 'executor'
          : 'planner'
    const hasToolResult = body.messages.some(message => message.role === 'tool')
    if (prompt.includes('Return the final verification result now')
      || prompt.includes('Correct the rejected verification result now')) {
      verificationResultAttempts[role] += 1
      let valid = verificationJson(prompt, role)
      const malformedLimit = role === 'reviewer'
        ? (options.malformedReviewerResults ?? 0)
        : (options.malformedVerifierResults ?? 0)
      const mismatchedEvidenceLimit = role === 'reviewer'
        ? (options.mismatchedReviewerEvidenceResults ?? 0)
        : (options.mismatchedVerifierEvidenceResults ?? 0)
      if (verificationResultAttempts[role] <= mismatchedEvidenceLimit) {
        const parsed = JSON.parse(valid)
        for (const finding of parsed.findings) {
          for (const source of finding.evidence_sources) {
            source.type = source.type === 'command' ? 'test' : 'command'
          }
        }
        valid = JSON.stringify(parsed)
      } else if (mismatchedEvidenceLimit > 0 && verificationResultAttempts[role] === 2) {
        assert.match(prompt, /opaque system label/u)
        assert.match(prompt, /byte-for-byte copy/u)
        assert.match(prompt, /Rejected citation details:/u)
        assert.match(prompt, /allowed_for_event/u)
      }
      const text = verificationResultAttempts[role] <= malformedLimit
        ? valid.replace(
            `${role} observed the passing fixture check.`,
            `${role} observed the "passing" fixture check.`,
          )
        : valid
      writeStream(response, body.model, { type: 'text', text })
      return
    }
    if (role === 'executor' && !hasToolResult) {
      writeStream(response, body.model, {
        type: 'tool',
        calls: [{
          id: 'call-executor-inspect',
          name: commandTool(body),
          arguments: {
            cmd: [
              'test -z "${WINWINCODE_EVALUATION_TEST_API_KEY+x}"',
              'test -f src/value.mjs',
            ].join(' && '),
          },
        }, {
          id: 'call-executor-apply',
          name: commandTool(body),
          arguments: {
            yield_time_ms: 250,
            cmd: [
              'sleep 2',
              'test -z "${WINWINCODE_EVALUATION_TEST_API_KEY+x}"',
              'printf "executor artifact\\n" > executor-only.tmp',
              `printf "export const value = 'after'\\n" > src/value.mjs`,
              'env -u NODE_TEST_CONTEXT node --test',
            ].join(' && '),
          },
        }],
      })
      return
    }
    if ((role === 'reviewer' || role === 'verifier') && !hasToolResult) {
      writeStream(response, body.model, {
        type: 'tool',
        id: `call-${role}`,
        name: commandTool(body),
        arguments: {
          cmd: [
            'test -z "${WINWINCODE_EVALUATION_TEST_API_KEY+x}"',
            'test ! -e executor-only.tmp',
            'env -u NODE_TEST_CONTEXT node --test',
          ].join(' && '),
        },
      })
      return
    }
    writeStream(response, body.model, { type: 'text', text: `${role} turn completed.` })
  })
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  const address = server.address()
  return Object.freeze({
    server,
    requests,
    backgroundPolls: () => backgroundPolls,
    parallelToolResultFollowUps: () => parallelToolResultFollowUps,
    verificationResultAttempts: role => verificationResultAttempts[role],
    baseURL: `http://127.0.0.1:${String(address.port)}/v1`,
  })
}

test('CLI refuses live evaluation without explicit opt-in', () => {
  const result = spawnSync(process.execPath, ['scripts/run-live-evaluation.mjs'], {
    cwd: root,
    encoding: 'utf8',
  })
  assert.equal(result.status, 2)
  assert.match(result.stderr, /explicit --live opt-in is required/u)
})

test('treats an already-removed DSH role session as completed cleanup only', async () => {
  await assert.doesNotReject(disposeCompletedRoleSession({
    async dispose() {
      throw Object.assign(new Error('session already removed'), { code: 'SESSION_NOT_FOUND' })
    },
  }))
  await assert.rejects(
    disposeCompletedRoleSession({
      async dispose() {
        throw Object.assign(new Error('ledger write failed'), { code: 'LEDGER_WRITE_FAILED' })
      },
    }),
    error => error?.code === 'LEDGER_WRITE_FAILED',
  )
})

test('CLI SIGINT leaves an interrupted result and exits with 130', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'winwincode-live-cli-signal-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const repository = await fixtureRepository(directory)
  const configPath = join(directory, 'config.json')
  const output = join(directory, 'results')
  const config = configFor(repository, 'http://127.0.0.1:1/v1')
  await writeFile(configPath, `${JSON.stringify(config)}\n`)
  const child = spawn(process.execPath, [
    'scripts/run-live-evaluation.mjs',
    '--live',
    '--config',
    configPath,
    '--output',
    output,
  ], {
    cwd: root,
    env: {
      ...process.env,
      WINWINCODE_EVALUATION_TEST_API_KEY: 'fixture-cli-signal-key',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  t.after(() => child.kill('SIGKILL'))
  const stdout = []
  const stderr = []
  child.stdout.on('data', chunk => stdout.push(chunk))
  child.stderr.on('data', chunk => stderr.push(chunk))
  const resultPath = join(output, config.runId, 'result.json')
  await waitForFile(resultPath)
  child.kill('SIGINT')
  const [code, signal] = await once(child, 'close')
  assert.equal(code, 130, Buffer.concat(stderr).toString('utf8'))
  assert.equal(signal, null)
  const result = JSON.parse(await readFile(resultPath, 'utf8'))
  assert.equal(result.state, 'interrupted')
  assert.match(Buffer.concat(stderr).toString('utf8'), /"resultPath":/u)
  assert.equal(Buffer.concat(stdout).toString('utf8'), '')
})

test('configuration requires bounded calls, turns, tokens, cost, and an exact repository base', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'winwincode-live-config-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const repository = await fixtureRepository(directory)
  const config = configFor(repository, 'http://127.0.0.1:12345/v1')
  assert.equal(parseLiveEvaluationConfig(config).deliverySpec.baseRevision, repository.commit)
  assert.throws(
    () => parseLiveEvaluationConfig({
      ...config,
      budgets: { ...config.budgets, maxTurns: 5 },
    }),
    error => error instanceof LiveEvaluationError && error.code === 'INVALID_CONFIG',
  )
  assert.throws(
    () => parseLiveEvaluationConfig({ ...config, rawApiKey: 'secret' }),
    error => error instanceof LiveEvaluationError && error.code === 'INVALID_CONFIG',
  )
  assert.throws(
    () => parseLiveEvaluationConfig({ ...config, runId: 'run/../../outside-output' }),
    error => error instanceof LiveEvaluationError && error.code === 'INVALID_CONFIG',
  )
  assert.throws(
    () => parseLiveEvaluationConfig({
      ...config,
      provider: { ...config.provider, baseURL: `${config.provider.baseURL}?token=secret` },
    }),
    error => error instanceof LiveEvaluationError && error.code === 'INVALID_CONFIG',
  )
  assert.throws(
    () => parseLiveEvaluationConfig({
      ...config,
      provider: { ...config.provider, api: 'openai-completions' },
    }),
    error => error instanceof LiveEvaluationError && error.code === 'INVALID_CONFIG',
  )
  assert.throws(
    () => parseLiveEvaluationConfig({
      ...config,
      provider: { ...config.provider, apiKeyEnv: 'PROVIDER_PASSWORD' },
    }),
    error => error instanceof LiveEvaluationError && error.code === 'INVALID_CONFIG',
  )
  assert.throws(
    () => parseLiveEvaluationConfig({
      ...config,
      execution: { commitMessage: 'first line\nsecond line' },
    }),
    error => error instanceof LiveEvaluationError && error.code === 'INVALID_CONFIG',
  )
  const embeddedCredential = 'fixture-"quoted\\credential-value'
  await assert.rejects(
    runLiveEvaluation({
      optIn: true,
      config: {
        ...config,
        humanDecisions: {
          ...config.humanDecisions,
          planReview: {
            ...config.humanDecisions.planReview,
            comments: `credential must be rejected: ${embeddedCredential}`,
          },
        },
      },
      outputDirectory: join(directory, 'rejected-result'),
      environment: {
        WINWINCODE_EVALUATION_TEST_API_KEY: embeddedCredential,
      },
    }),
    error => error instanceof LiveEvaluationError && error.code === 'RAW_CREDENTIAL_REJECTED',
  )
  await assert.rejects(
    runLiveEvaluation({
      optIn: true,
      config,
      outputDirectory: join(directory, 'preflight-bypass-result'),
      environment: {
        WINWINCODE_EVALUATION_TEST_API_KEY: 'fixture-preflight-bypass-key',
      },
      preflight: async () => ({ status: 'passed' }),
    }),
    error => error instanceof LiveEvaluationError && error.code === 'INVALID_OPTIONS',
  )
  const lockedOutput = join(directory, 'locked-result')
  await mkdir(lockedOutput)
  const activeLock = join(lockedOutput, `${config.runId}.lock`)
  await writeFile(activeLock, 'active-run\n')
  await assert.rejects(
    runLiveEvaluation({
      optIn: true,
      config,
      outputDirectory: lockedOutput,
      environment: {
        WINWINCODE_EVALUATION_TEST_API_KEY: 'fixture-active-run-key',
      },
    }),
    error => error instanceof LiveEvaluationError && error.code === 'RUN_EXISTS',
  )
  assert.equal(await readFile(activeLock, 'utf8'), 'active-run\n')
})

test('runs one isolated canonical Delivery through the real DSH provider route', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'winwincode-live-run-'))
  if (process.env.WINWINCODE_KEEP_EVALUATION_FIXTURE !== '1') {
    t.after(() => rm(directory, { recursive: true, force: true }))
  } else {
    process.stderr.write(`kept live evaluation fixture: ${directory}\n`)
  }
  const repository = await fixtureRepository(directory)
  const provider = await fakeOpenAiServer({
    malformedReviewerResults: 1,
    mismatchedVerifierEvidenceResults: 1,
  })
  t.after(() => new Promise(resolveClose => provider.server.close(resolveClose)))
  const output = join(directory, 'results')
  const credential = 'fixture-live-provider-key-that-must-not-leak'
  const result = await runLiveEvaluation({
    optIn: true,
    config: configFor(repository, provider.baseURL),
    outputDirectory: output,
    environment: {
      ...process.env,
      WINWINCODE_EVALUATION_TEST_API_KEY: credential,
    },
  })
  assert.equal(result.result.state, 'completed')
  assert.equal(result.result.delivery.status, 'delivered')
  assert.equal(result.result.delivery.verdict.status, 'pass')
  assert.equal(result.result.delivery.stageRuns.length, 6)
  assert.equal(result.result.delivery.sessionBindings.length, 6)
  assert.equal(result.result.inputs.deliverySpec.id, 'spec-live-evaluation-fixture')
  assert.equal(result.result.inputs.deliverySpec.revision, 2)
  assert.equal(result.result.inputs.solution.id, 'solution-live-evaluation-fixture')
  assert.match(result.result.inputs.deliverySpecSha256, /^[0-9a-f]{64}$/u)
  assert.match(result.result.inputs.solutionSha256, /^[0-9a-f]{64}$/u)
  assert.match(result.result.inputs.humanDecisionsSha256, /^[0-9a-f]{64}$/u)
  assert.equal(result.result.candidate.baseCommitId, repository.commit)
  assert.match(result.result.candidate.candidateCommitId, /^[0-9a-f]{40}$/u)
  assert.match(result.result.candidate.candidateTreeId, /^[0-9a-f]{40}$/u)
  assert.match(result.result.candidate.diffSha256, /^[0-9a-f]{64}$/u)
  assert.equal(result.result.provider.catalog, 'dsh-pi-ai')
  assert.equal(result.result.provider.route, 'deepseek')
  assert.equal(result.result.provider.model, 'deepseek-v4-flash')
  assert.equal(result.result.provider.endpointOverride, provider.baseURL)
  assert.equal(result.result.provider.modelInfo.contextWindow, 1_000_000)
  assert.equal(result.result.provider.modelInfo.inputModalities.includes('text'), true)
  assert.equal(result.result.provider.modelInfo.reasoningEfforts.includes('high'), true)
  assert.equal(
    result.result.provider.credentialRef,
    'WINWINCODE_EVALUATION_TEST_API_KEY',
  )
  assert.match(result.result.sourceIdentities.codex.commit, /^[0-9a-f]{40}$/u)
  assert.match(result.result.sourceIdentities.dsh.commit, /^[0-9a-f]{40}$/u)
  assert.equal(
    result.result.sourceIdentities.project.repository,
    'https://github.com/changw98ic/winwincode',
  )
  assert.match(
    result.result.sourceIdentities.project.releaseSourceSha256,
    /^[0-9a-f]{64}$/u,
  )
  assert.match(result.result.sourceIdentities.evaluator.runnerSha256, /^[0-9a-f]{64}$/u)
  assert.match(
    result.result.sourceIdentities.evaluator.measuresAdapterSha256,
    /^[0-9a-f]{64}$/u,
  )
  assert.match(
    result.result.sourceIdentities.evaluator.measuresCliSha256,
    /^[0-9a-f]{64}$/u,
  )
  assert.match(
    result.result.sourceIdentities.evaluator.measuresProjectionSha256,
    /^[0-9a-f]{64}$/u,
  )
  assert.match(
    result.result.sourceIdentities.evaluator.measuresRuntimeSha256,
    /^[0-9a-f]{64}$/u,
  )
  assert.match(result.result.sourceIdentities.native.buildInfoSha256, /^[0-9a-f]{64}$/u)
  assert.equal(result.result.preflight.status, 'passed')
  assert.notEqual(result.result.repository.workspace, result.result.repository.reviewWorkspace)
  assert.equal(
    checked('git', ['status', '--porcelain=v1'], {
      cwd: result.result.repository.reviewWorkspace,
    }),
    '',
  )
  await assert.rejects(readFile(join(
    result.result.repository.reviewWorkspace,
    'executor-only.tmp',
  )), error => error?.code === 'ENOENT')
  assert.equal(
    checked('git', ['rev-parse', 'HEAD'], { cwd: repository.repository }),
    repository.commit,
  )
  assert.equal(checked('git', ['status', '--porcelain=v1'], {
    cwd: repository.repository,
  }), '')
  assert.equal(
    await readFile(join(repository.repository, 'src', 'value.mjs'), 'utf8'),
    "export const value = 'before'\n",
  )
  assert.equal(result.result.budget.turns, 8)
  assert.equal(result.result.budget.modelCalls >= 9, true)
  assert.equal(result.result.budget.limits.pricing.source, 'fixture pricing')
  assert.equal(result.result.delivery.stageRuns.every(stageRun => {
    const binding = result.result.delivery.sessionBindings.find(entry => (
      entry.stageRunId === stageRun.id
    ))
    return binding !== undefined
      && binding.dshSessionId !== null
      && (stageRun.stage === 'plan-review' || stageRun.stage === 'delivery-review'
        ? binding.codexSessionId === null
        : binding.codexSessionId !== null)
  }), true)
  assert.equal(result.result.runtimeProjection.stages.some(stage => (
    stage.sessions.some(session => session.agents.length > 0)
  )), true)
  assert.equal(result.result.measures.runKind, 'live')
  assert.equal(result.result.measures.runId, result.result.runId)
  assert.equal(result.result.measures.outcome.classification.value, 'proven-success')
  assert.equal(
    result.result.measures.dimensions.completeness.status.value,
    'complete',
  )
  assert.equal(
    result.result.measures.dimensions.confidence.status.value,
    'independently-supported',
  )
  assert.equal(
    result.result.measures.dimensions.efficiency.modelCallCount.value,
    result.result.budget.modelCalls,
  )
  assert.equal(
    result.result.measures.dimensions.efficiency.totalTokens.value,
    result.result.budget.usage.totalTokens,
  )
  assert.equal(
    result.result.measures.dimensions.efficiency.costUsdMicros.value,
    result.result.budget.usage.costUsdMicros,
  )
  assert.equal(
    result.result.measures.dimensions.efficiency.runElapsedMillis.value,
    result.result.finishedAtMillis - result.result.startedAtMillis,
  )
  assert.deepEqual(measureLiveEvaluationResult(result.result), result.result.measures)
  assertMeasureSources(result.result.measures)
  const measuredByCli = spawnSync(process.execPath, [
    resolve(root, 'scripts/run-evaluation-measures.mjs'),
    '--result',
    result.path,
    '--check',
  ], { cwd: root, encoding: 'utf8' })
  assert.equal(measuredByCli.status, 0, measuredByCli.stderr)
  assert.deepEqual(JSON.parse(measuredByCli.stdout), result.result.measures)
  const stored = await readFile(result.path, 'utf8')
  assert.equal(stored.includes(credential), false)
  assert.equal(stored.includes('credentialRef'), true)
  await assert.rejects(
    runLiveEvaluation({
      optIn: true,
      config: configFor(repository, provider.baseURL),
      outputDirectory: output,
      environment: {
        ...process.env,
        WINWINCODE_EVALUATION_TEST_API_KEY: credential,
      },
    }),
    error => error instanceof LiveEvaluationError && error.code === 'RUN_EXISTS',
  )
  assert.equal(await readFile(result.path, 'utf8'), stored)
  assert.equal(provider.requests.length, result.result.budget.modelCalls)
  assert.equal(provider.backgroundPolls() >= 1, true)
  assert.equal(provider.parallelToolResultFollowUps() >= 1, true)
  assert.equal(provider.verificationResultAttempts('reviewer'), 2)
  assert.equal(provider.verificationResultAttempts('verifier'), 2)
})

test('fails closed after the bounded verification-result correction is exhausted', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'winwincode-live-result-correction-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const repository = await fixtureRepository(directory)
  const provider = await fakeOpenAiServer({ mismatchedReviewerEvidenceResults: 2 })
  t.after(() => new Promise(resolveClose => provider.server.close(resolveClose)))
  let failure
  try {
    await runLiveEvaluation({
      optIn: true,
      config: {
        ...configFor(repository, provider.baseURL),
        runId: 'live-evaluation-result-correction-exhausted',
      },
      outputDirectory: join(directory, 'results'),
      environment: {
        ...process.env,
        WINWINCODE_EVALUATION_TEST_API_KEY: 'fixture-correction-limit-key',
      },
    })
  } catch (error) {
    failure = error
  }

  assert.equal(failure?.code, 'VERIFICATION_RESULT_INVALID')
  const result = JSON.parse(await readFile(failure.evaluationResultPath, 'utf8'))
  assert.equal(result.state, 'failed')
  assert.equal(result.error.code, 'VERIFICATION_RESULT_INVALID')
  assert.equal(result.delivery.verdict, null)
  assert.equal(result.delivery.evidence.length, 0)
  assert.equal(provider.verificationResultAttempts('reviewer'), 2)
  assert.equal(provider.verificationResultAttempts('verifier'), 0)
})

test('persists a sanitized inspectable result when the DSH provider route fails', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'winwincode-live-failure-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const repository = await fixtureRepository(directory)
  const credential = 'fixture-provider-failure-secret-that-must-not-leak'
  const provider = await rejectingOpenAiServer(credential)
  const config = configFor(repository, provider.baseURL)
  t.after(() => new Promise(resolveClose => provider.server.close(resolveClose)))
  let failure
  try {
    await runLiveEvaluation({
      optIn: true,
      config,
      outputDirectory: join(directory, 'results'),
      environment: {
        ...process.env,
        WINWINCODE_EVALUATION_TEST_API_KEY: credential,
      },
    })
  } catch (error) {
    failure = error
  }
  assert.equal(failure?.code, 'CODEX_TURN_FAILED')
  assert.equal(provider.requestCount() > 0, true)
  assert.equal(typeof failure.evaluationResultPath, 'string')
  const stored = await readFile(failure.evaluationResultPath, 'utf8')
  const result = JSON.parse(stored)
  assert.equal(result.state, 'failed')
  assert.equal(result.phase, 'failed')
  assert.equal(result.preflight.status, 'passed')
  assert.notEqual(result.sourceIdentities, null)
  assert.deepEqual({
    catalog: result.provider.catalog,
    route: result.provider.route,
    model: result.provider.model,
    credentialRef: result.provider.credentialRef,
    endpointOverride: result.provider.endpointOverride,
    reasoningEffort: result.provider.reasoningEffort,
  }, {
    catalog: 'dsh-pi-ai',
    route: config.provider.route,
    model: config.provider.model,
    credentialRef: config.provider.apiKeyEnv,
    endpointOverride: config.provider.baseURL,
    reasoningEffort: config.provider.reasoningEffort,
  })
  assert.notEqual(result.provider.modelInfo, null)
  assert.equal(result.error.code, 'CODEX_TURN_FAILED')
  assert.notEqual(result.runtimeProjection, null)
  assert.notEqual(result.measures, null)
  assert.deepEqual(measureLiveEvaluationResult(result), result.measures)
  assert.equal(result.measures.runKind, 'live')
  assert.equal(stored.includes(credential), false)
  assert.equal(stored.includes('provider rejected'), false)
})

test('persists an inspectable interrupted result after the provider route starts', async t => {
  const directory = await mkdtemp(join(tmpdir(), 'winwincode-live-interrupted-'))
  t.after(() => rm(directory, { recursive: true, force: true }))
  const repository = await fixtureRepository(directory)
  const controller = new AbortController()
  const provider = await interruptingOpenAiServer(() => {
    controller.abort(new LiveEvaluationError('INTERRUPTED', 'fixture interruption'))
  })
  t.after(() => new Promise(resolveClose => provider.server.close(resolveClose)))
  const evaluation = runLiveEvaluation({
    optIn: true,
    config: configFor(repository, provider.baseURL),
    outputDirectory: join(directory, 'results'),
    environment: {
      ...process.env,
      WINWINCODE_EVALUATION_TEST_API_KEY: 'fixture-interruption-key',
    },
    signal: controller.signal,
  }).then(
    () => undefined,
    error => error,
  )
  const routeOutcome = await Promise.race([
    provider.requestStarted.then(() => ({ kind: 'request' })),
    evaluation.then(error => ({ kind: 'evaluation', error })),
  ])
  assert.equal(
    routeOutcome.kind,
    'request',
    `evaluation ended before the provider route: ${routeOutcome.error?.code ?? 'completed'}`,
  )
  const interruptedWhenRouteStarted = controller.signal.aborted
  const failure = await evaluation
  assert.equal(interruptedWhenRouteStarted, true)
  assert.equal(provider.requestCount() > 0, true)
  assert.equal(provider.interruptCount(), 1)
  assert.equal(controller.signal.aborted, true)
  assert.equal(controller.signal.reason?.code, 'INTERRUPTED')
  assert.equal(failure?.code, 'CODEX_TURN_FAILED')
  assert.equal(typeof failure.evaluationResultPath, 'string')
  const stored = await readFile(failure.evaluationResultPath, 'utf8')
  const result = JSON.parse(stored)
  assert.equal(result.state, 'interrupted')
  assert.equal(result.phase, 'interrupted')
  assert.equal(result.preflight.status, 'passed')
  assert.notEqual(result.delivery, null)
  assert.notEqual(result.runtimeProjection, null)
  assert.notEqual(result.measures, null)
  assert.deepEqual(measureLiveEvaluationResult(result), result.measures)
  assert.equal(result.measures.outcome.successClaimPresent.value, false)
  assert.deepEqual(result.error, {
    phase: 'planning',
    code: 'INTERRUPTED',
    category: 'evaluation',
  })
  assert.equal(stored.includes('fixture-interruption-key'), false)
})
