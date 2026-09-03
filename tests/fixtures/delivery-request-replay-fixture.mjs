import { spawnSync } from 'node:child_process'
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

import {
  DELIVERY_SCHEMA_VERSION,
  materializeStrongFlowDeliveryRequest,
} from '../../packages/contracts/dist/index.js'
import {
  DeliveryStore,
  StrongFlowService,
  StrongFlowServiceInvoker,
  createStrongFlowDeliveryLocalProofAuthenticator,
} from '../../packages/strongflow/dist/index.js'

const DELIVERY_FIXTURE_BASE_TIME = 2_900_000_000_000
const DELIVERY_FIXTURE_UI_PROOF = 'fixture-local-session-proof-value'
const DELIVERY_FIXTURE_CLI_PROOF = 'fixture-local-peer-proof-value'
const DEFAULT_DELIVERY_ID = 'dlv_01J00000000000000000000007'

function immutable(value) {
  const clone = structuredClone(value)
  const pending = []
  if (typeof clone === 'object' && clone !== null) pending.push(clone)
  while (pending.length > 0) {
    const current = pending.pop()
    if (Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

function checked(command, arguments_, options = {}) {
  const result = spawnSync(command, arguments_, {
    cwd: options.cwd,
    encoding: 'utf8',
    env: options.env ?? process.env,
    timeout: options.timeout ?? 30_000,
  })
  if (result.error !== undefined) throw result.error
  if (result.signal !== null || result.status !== 0) {
    throw new Error([
      `${command} ${arguments_.join(' ')} failed`,
      `signal=${result.signal ?? 'none'}`,
      `status=${String(result.status)}`,
      result.stderr.trim(),
      result.stdout.trim(),
    ].filter(Boolean).join('\n'))
  }
  return result.stdout
}

function git(repository, ...arguments_) {
  return checked('git', arguments_, { cwd: repository }).trim()
}

class DeterministicReplayClock {
  #value

  constructor(start = DELIVERY_FIXTURE_BASE_TIME + 100) {
    if (!Number.isSafeInteger(start) || start < 0) {
      throw new TypeError('replay fixture clock start must be a non-negative safe integer')
    }
    this.#value = start
    this.now = this.now.bind(this)
  }

  now() {
    this.#value += 1
    return this.#value
  }

  peek() {
    return this.#value
  }
}

async function initializeRepository(repository) {
  await mkdir(join(repository, 'src'), { recursive: true })
  await mkdir(join(repository, 'test'), { recursive: true })
  await writeFile(join(repository, 'src', 'value.mjs'), "export const value = 'before'\n")
  await writeFile(join(repository, 'test', 'value.test.mjs'), [
    "import assert from 'node:assert/strict'",
    "import test from 'node:test'",
    "import { value } from '../src/value.mjs'",
    '',
    "test('fixture candidate', () => { assert.equal(value, 'after') })",
    '',
  ].join('\n'))
  await writeFile(join(repository, 'package.json'), `${JSON.stringify({
    name: 'winwincode-delivery-fixture-repository',
    private: true,
    type: 'module',
    scripts: { test: 'node --test' },
  }, null, 2)}\n`)
  git(repository, 'init', '--initial-branch=main')
  git(repository, 'config', 'user.name', 'WinWinCode Fixture')
  git(repository, 'config', 'user.email', 'fixture@winwincode.invalid')
  git(repository, 'add', '--all')
  checked('git', ['commit', '-m', 'Create deterministic fixture baseline'], {
    cwd: repository,
    env: {
      ...process.env,
      GIT_AUTHOR_DATE: '2025-01-01T00:00:00Z',
      GIT_COMMITTER_DATE: '2025-01-01T00:00:00Z',
    },
  })
  return Object.freeze({
    baseCommitId: git(repository, 'rev-parse', 'HEAD'),
    baseTreeId: git(repository, 'rev-parse', 'HEAD^{tree}'),
  })
}

async function readRepositoryIdentity(repository) {
  return Object.freeze({
    baseCommitId: git(repository, 'rev-list', '--max-parents=0', 'HEAD'),
    baseTreeId: git(repository, 'rev-parse', `${git(repository, 'rev-list', '--max-parents=0', 'HEAD')}^{tree}`),
  })
}

export class DeliveryRequestReplayFixture {
  #cleanupPromise
  #ownsRoot

  constructor(options) {
    this.root = resolve(options.root)
    this.home = join(this.root, 'home')
    this.repository = join(this.root, 'repository')
    this.repositoryLocator = options.repositoryLocator ?? this.repository
    this.deliveryId = options.deliveryId ?? DEFAULT_DELIVERY_ID
    this.clock = new DeterministicReplayClock(options.clockStart)
    this.repositoryIdentity = options.repositoryIdentity
    this.diagramFacts = { runtimeEvents: Object.freeze([]), candidate: null }
    this.authenticator = createStrongFlowDeliveryLocalProofAuthenticator({
      localSessionProof: DELIVERY_FIXTURE_UI_PROOF,
      localPeerProof: DELIVERY_FIXTURE_CLI_PROOF,
      localSessionActorId: 'fixture-ui-reviewer',
      localPeerActorId: 'fixture-cli-reviewer',
    })
    this.service = new StrongFlowService({
      home: this.home,
      authenticator: this.authenticator,
      clock: this.clock.now,
      executionSource: {
        read: async () => this.diagramFacts,
      },
    })
    this.invoker = new StrongFlowServiceInvoker(this.service)
  }

  static async create(options = {}) {
    const ownsRoot = options.root === undefined
    const root = options.root === undefined
      ? await mkdtemp(join(tmpdir(), 'winwincode-delivery-replay-'))
      : resolve(options.root)
    await mkdir(root, { recursive: true })
    const repository = join(root, 'repository')
    let repositoryIdentity
    try {
      repositoryIdentity = await readRepositoryIdentity(repository)
    } catch {
      repositoryIdentity = await initializeRepository(repository)
    }
    await mkdir(join(root, 'home'), { recursive: true })
    return new DeliveryRequestReplayFixture({
      ...options,
      root,
      ownsRoot,
      repositoryIdentity,
    })
  }

  spec(revision, suffix = `v${String(revision)}`) {
    return immutable({
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: `spec-${this.deliveryId}-${suffix}`,
      deliveryId: this.deliveryId,
      revision,
      title: `Deterministic Delivery ${suffix}`,
      goal: 'Prove the canonical Delivery path from reviewed goal to direct evidence.',
      scope: ['One local repository value change'],
      outOfScope: ['A second Agent scheduler', 'A generic task tracker'],
      constraints: [
        'Codex remains the execution authority',
        'DSH remains the interaction and model-provider shell',
      ],
      acceptanceCriteria: [{
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        id: `criterion-${this.deliveryId}-${suffix}`,
        description: 'The local fixture exports the reviewed value.',
        verificationMethod: 'Run the local Node test against the frozen candidate.',
        required: true,
      }],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: this.repositoryLocator,
      },
      baseRevision: this.repositoryIdentity.baseCommitId,
      maxReworkAttempts: 2,
      createdAtMillis: DELIVERY_FIXTURE_BASE_TIME + revision,
    })
  }

  async request(operation, requestId, payload) {
    const request = materializeStrongFlowDeliveryRequest(operation, requestId, payload)
    return this.invoker.invoke(request)
  }

  async stored() {
    return DeliveryStore.open(this.home, this.deliveryId).then(store => store.read())
  }

  cleanup() {
    this.#cleanupPromise ??= rm(this.root, { recursive: true, force: true })
    return this.#cleanupPromise
  }
}
