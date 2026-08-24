import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { createServer } from 'node:http'
import test from 'node:test'

import { Context, Service } from '@deepseek-ai/cordis'

import {
  DshGitHubPublicationProvider,
} from '../packages/dsh-profile/dist/index.js'
import {
  STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
  STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
  parseStrongFlowGitHubProviderOperation,
} from '../packages/strongflow/dist/index.js'

const token = 'fixture-github-credential-value'
const repository = 'example/widget'
const commitId = '3'.repeat(40)
const providerIdempotencyKey = `github:pull-request:sha256:${createHash('sha256')
  .update('dsh-github-provider-fixture')
  .digest('hex')}`

class FixtureCredentials extends Service {
  value
  calls = []

  constructor(ctx, value) {
    super(ctx, 'credentials')
    this.value = value
  }

  async resolve(ref) {
    this.calls.push(ref)
    return this.value === null ? undefined : { value: this.value, source: 'fixture' }
  }

  async describe() {
    return { configured: this.value !== null, source: 'fixture', writable: false }
  }

  async set() {
    throw new Error('fixture credentials are read-only')
  }

  async unset() {
    throw new Error('fixture credentials are read-only')
  }
}

function operation(kind, payload, key = providerIdempotencyKey) {
  const unsigned = {
    schemaVersion: STRONGFLOW_GITHUB_PROVIDER_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PROVIDER_OPERATION_PROTOCOL,
    kind,
    operationKey: `${key}:${kind}`,
    payload,
  }
  return parseStrongFlowGitHubProviderOperation({
    ...unsigned,
    requestSha256: createHash('sha256').update(JSON.stringify(unsigned)).digest('hex'),
  })
}

function operations(overrides = {}) {
  const targetRepository = overrides.repository ?? repository
  const targetCommit = overrides.commitId ?? commitId
  const issueNumber = overrides.issueNumber ?? 42
  const branch = overrides.branch ?? 'winwincode/dsh-provider-fixture'
  const key = overrides.providerIdempotencyKey ?? providerIdempotencyKey
  const marker = `<!-- winwincode-publication:${key} -->`
  return [
    operation('branch', {
      repository: targetRepository,
      branch,
      commitId: targetCommit,
    }, key),
    operation('pull-request', {
      repository: targetRepository,
      baseBranch: overrides.baseBranch ?? 'main',
      headRepository: targetRepository,
      headBranch: branch,
      title: 'WinWinCode DSH GitHub provider integration',
      body: `${marker}\n\nExact provider integration fixture.`,
    }, key),
    operation('issue-comment', {
      repository: targetRepository,
      issueNumber,
      body: `${marker}\n\nWinWinCode provider integration fixture completed.`,
    }, key),
    operation('commit-status', {
      repository: targetRepository,
      commitId: targetCommit,
      context: 'winwincode/delivery',
      state: 'success',
      description: 'WinWinCode verified all required acceptance criteria.',
      targetUrl: `https://github.com/${targetRepository}/issues/${String(issueNumber)}`,
    }, key),
  ]
}

async function requestJson(request) {
  const chunks = []
  for await (const chunk of request) chunks.push(chunk)
  return JSON.parse(Buffer.concat(chunks).toString('utf8'))
}

function json(response, status, value) {
  const body = JSON.stringify(value)
  response.writeHead(status, {
    'Content-Type': 'application/json',
    'Content-Length': Buffer.byteLength(body),
  })
  response.end(body)
}

async function githubFixtureServer(t) {
  const state = {
    branch: null,
    pullRequest: null,
    comments: [],
    statuses: [],
    writes: [],
    requests: 0,
    authorized: 0,
    failStatus: null,
  }
  let baseUrl
  const server = createServer((request, response) => {
    void (async () => {
      state.requests += 1
      if (request.headers.authorization === `Bearer ${token}`) state.authorized += 1
      else return json(response, 401, { message: 'credential rejected' })
      if (state.failStatus !== null) {
        return json(response, state.failStatus, {
          message: `sensitive remote diagnostic ${token}`,
        })
      }
      const url = new URL(request.url, baseUrl)
      const path = decodeURIComponent(url.pathname)
      if (request.method === 'GET'
        && path === '/repos/example/widget/git/ref/heads/winwincode/dsh-provider-fixture') {
        return state.branch === null
          ? json(response, 404, { message: 'not found' })
          : json(response, 200, state.branch)
      }
      if (request.method === 'POST' && path === '/repos/example/widget/git/refs') {
        const body = await requestJson(request)
        state.branch = {
          ref: body.ref,
          object: { sha: body.sha },
          url: `${baseUrl}/resources/branch`,
        }
        state.writes.push('branch')
        return json(response, 201, state.branch)
      }
      if (request.method === 'GET' && path === '/repos/example/widget/pulls') {
        return json(response, 200, state.pullRequest === null ? [] : [state.pullRequest])
      }
      if (request.method === 'POST' && path === '/repos/example/widget/pulls') {
        const body = await requestJson(request)
        state.pullRequest = {
          title: body.title,
          body: body.body,
          head: { ref: 'winwincode/dsh-provider-fixture', repo: { full_name: 'Example/Widget' } },
          base: { ref: body.base, repo: { full_name: 'Example/Widget' } },
          html_url: `${baseUrl}/resources/pull-request`,
        }
        state.writes.push('pull-request')
        return json(response, 201, state.pullRequest)
      }
      if (request.method === 'GET' && path === '/repos/example/widget/issues/42/comments') {
        return json(response, 200, state.comments)
      }
      if (request.method === 'POST' && path === '/repos/example/widget/issues/42/comments') {
        const body = await requestJson(request)
        const comment = {
          body: body.body,
          html_url: `${baseUrl}/resources/issue-comment`,
        }
        state.comments.push(comment)
        state.writes.push('issue-comment')
        return json(response, 201, comment)
      }
      if (request.method === 'GET'
        && path === `/repos/example/widget/commits/${commitId}/statuses`) {
        return json(response, 200, [...state.statuses].reverse())
      }
      if (request.method === 'POST'
        && path === `/repos/example/widget/statuses/${commitId}`) {
        const body = await requestJson(request)
        const status = {
          state: body.state,
          target_url: body.target_url,
          description: body.description,
          context: body.context,
          url: `${baseUrl}/resources/commit-status`,
        }
        state.statuses.push(status)
        state.writes.push('commit-status')
        return json(response, 201, status)
      }
      return json(response, 404, { message: 'fixture route not found' })
    })().catch(() => json(response, 500, { message: 'fixture server failed' }))
  })
  await new Promise((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolve)
  })
  const address = server.address()
  assert.notEqual(address, null)
  assert.equal(typeof address, 'object')
  baseUrl = `http://127.0.0.1:${String(address.port)}`
  t.after(() => new Promise((resolve, reject) => {
    server.close(error => { if (error === undefined) resolve(); else reject(error) })
  }))
  return { baseUrl, state }
}

async function mountProvider(t, config) {
  const ctx = new Context()
  const credentials = new FixtureCredentials(ctx, config.token)
  const provider = new DshGitHubPublicationProvider(ctx, {
    apiBaseUrl: config.apiBaseUrl,
    requestTimeoutMillis: 2_000,
    maxLookupPages: 2,
  })
  t.after(() => ctx.fiber.dispose())
  return { credentials, provider }
}

async function applyOrReconcile(provider, entries) {
  const results = []
  for (const entry of entries) {
    const observation = await provider.lookup(entry)
    if (observation.state === 'found') {
      results.push(observation)
      continue
    }
    assert.equal(observation.state, 'absent', JSON.stringify(observation))
    const mutation = await provider.apply(entry)
    assert.equal(mutation.state, 'applied', JSON.stringify(mutation))
    results.push(mutation)
  }
  return results
}

test('DSH credential adapter applies and then reconciles all four GitHub operations', async t => {
  const fixture = await githubFixtureServer(t)
  const { credentials, provider } = await mountProvider(t, {
    apiBaseUrl: fixture.baseUrl,
    token,
  })
  const entries = operations()
  const applied = await applyOrReconcile(provider, entries)
  assert.equal(applied.every(result => result.state === 'applied'), true)
  assert.deepEqual(fixture.state.writes, [
    'branch',
    'pull-request',
    'issue-comment',
    'commit-status',
  ])

  const reconciled = await applyOrReconcile(provider, entries)
  assert.equal(reconciled.every(result => result.state === 'found'), true)
  assert.deepEqual(fixture.state.writes, [
    'branch',
    'pull-request',
    'issue-comment',
    'commit-status',
  ])
  assert.equal(fixture.state.authorized, fixture.state.requests)
  assert.equal(credentials.calls.length, fixture.state.requests)
  assert.equal(credentials.calls.every(ref => ref === 'GITHUB_TOKEN'), true)
  assert.equal(JSON.stringify({ applied, reconciled }).includes(token), false)
})

test('DSH GitHub provider keeps missing credentials and remote diagnostics sanitized', async t => {
  const fixture = await githubFixtureServer(t)
  const missing = await mountProvider(t, { apiBaseUrl: fixture.baseUrl, token: null })
  const [branch] = operations()
  const absentCredential = await missing.provider.lookup(branch)
  assert.deepEqual(absentCredential, {
    state: 'unknown',
    operationKey: branch.operationKey,
    code: 'credential-not-configured',
  })
  assert.equal(fixture.state.requests, 0)

  const configured = await mountProvider(t, { apiBaseUrl: fixture.baseUrl, token })
  fixture.state.failStatus = 500
  const remoteFailure = await configured.provider.lookup(branch)
  assert.deepEqual(remoteFailure, {
    state: 'unknown',
    operationKey: branch.operationKey,
    code: 'github-http-500',
  })
  assert.equal(JSON.stringify(remoteFailure).includes(token), false)
  assert.throws(
    () => new DshGitHubPublicationProvider(new Context(), {
      apiBaseUrl: `https://user:${token}@api.github.com`,
    }),
    /credential-free HTTPS/u,
  )
})

const liveEnvironment = {
  enabled: process.env.WINWINCODE_GITHUB_LIVE_TEST === '1',
  token: process.env.WINWINCODE_GITHUB_LIVE_TOKEN,
  repository: process.env.WINWINCODE_GITHUB_LIVE_REPOSITORY,
  commitId: process.env.WINWINCODE_GITHUB_LIVE_COMMIT,
  issueNumber: process.env.WINWINCODE_GITHUB_LIVE_ISSUE,
  baseBranch: process.env.WINWINCODE_GITHUB_LIVE_BASE_BRANCH,
  headBranch: process.env.WINWINCODE_GITHUB_LIVE_HEAD_BRANCH,
}
const liveReady = liveEnvironment.enabled
  && Object.values(liveEnvironment).every(value => value !== undefined && value !== '')

test('optional live DSH GitHub lane applies or reconciles the configured publication set', {
  skip: liveReady ? false : 'set the explicit WINWINCODE_GITHUB_LIVE_* inputs to enable',
}, async t => {
  const issueNumber = Number(liveEnvironment.issueNumber)
  assert.equal(Number.isSafeInteger(issueNumber) && issueNumber > 0, true)
  const liveKey = `github:pull-request:sha256:${createHash('sha256')
    .update([
      liveEnvironment.repository,
      liveEnvironment.commitId,
      liveEnvironment.issueNumber,
      liveEnvironment.baseBranch,
      liveEnvironment.headBranch,
    ].join('\n'))
    .digest('hex')}`
  const { provider } = await mountProvider(t, {
    apiBaseUrl: 'https://api.github.com',
    token: liveEnvironment.token,
  })
  const entries = operations({
    repository: liveEnvironment.repository,
    commitId: liveEnvironment.commitId,
    issueNumber,
    baseBranch: liveEnvironment.baseBranch,
    branch: liveEnvironment.headBranch,
    providerIdempotencyKey: liveKey,
  })
  await applyOrReconcile(provider, entries)
  const replay = await applyOrReconcile(provider, entries)
  assert.equal(replay.every(result => result.state === 'found'), true)
})
