// SPDX-License-Identifier: Apache-2.0

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { closeSync, existsSync, linkSync, lstatSync, mkdirSync, openSync, readFileSync,
  realpathSync, rmSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { checkedWwc, configuredDeviceModelRoute, establishDeviceOnlyExecutionPath,
  registerOrReuseDeviceRepository, seedDeviceLocalProvider, waitFor } from './device-production-fixture.mjs'
import { loadDeviceAgentTask } from './device-agent-task.mjs'
import { runApiProductionVertical } from './run-api-production-vertical.mjs'
import { benchmarkDeviceEnvironment, deviceProviderSecretValues } from './run-device-task-vertical.mjs'
import { assertDeviceAgentEnvironmentForwarded, deviceBenchmarkProfileKey } from './device-agent-environment.mjs'

// Stable protocol identities for an already frozen launch, not a retry attempt.
export function deviceTaskIdentities(experimentId, callId) {
  assert.ok(typeof experimentId === 'string' && experimentId.length > 0)
  assert.ok(typeof callId === 'string' && callId.length > 0)
  const bytes = createHash('sha256').update(JSON.stringify([experimentId, callId])).digest()
  const alphabet = '0123456789ABCDEFGHJKMNPQRSTVWXYZ'
  let value = BigInt(`0x${bytes.subarray(0, 16).toString('hex')}`)
  let suffix = ''
  for (let i = 0; i < 26; i++) { suffix = alphabet[Number(value & 31n)] + suffix; value >>= 5n }
  return { productSessionId: `psn_${suffix}`, deliveryId: `dlv_${suffix}` }
}

// Put each frozen task tree in the one registered repository. A private Git
// index avoids changing the shared checkout while other Sessions execute.
export function prepareDeviceTaskBaseline(repository, taskInputPath, directory) {
  const { task, digest } = loadDeviceAgentTask(taskInputPath)
  const files = {
    'package.json': `${JSON.stringify({ name: 'winwincode-api-fixture', private: true,
      scripts: { verify: "test -s .winwincode-api-candidate && printf '%s\\n' 'fixture verified'" } }, null, 2)}\n`,
    'package-lock.json': '{}\n',
    '.winwincode-api-candidate': 'deterministic StrongFlow candidate baseline\n',
    ...task.files,
  }
  mkdirSync(directory, { recursive: true, mode: 0o700 })
  const index = join(directory, `index-${digest}`)
  const env = { PATH: process.env.PATH, GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null',
    GIT_INDEX_FILE: index, GIT_AUTHOR_NAME: 'WinWinCode frozen task',
    GIT_AUTHOR_EMAIL: 'task@winwincode.invalid', GIT_COMMITTER_NAME: 'WinWinCode frozen task',
    GIT_COMMITTER_EMAIL: 'task@winwincode.invalid', GIT_AUTHOR_DATE: '2000-01-01T00:00:00Z',
    GIT_COMMITTER_DATE: '2000-01-01T00:00:00Z' }
  const git = (args, input) => execFileSync('git', ['-C', repository, ...args], {
    env, input, encoding: 'utf8', stdio: ['pipe', 'pipe', 'pipe'],
  }).trim()
  try {
    git(['read-tree', '--empty'])
    for (const [path, content] of Object.entries(files).sort(([a], [b]) => a.localeCompare(b))) {
      const blob = git(['hash-object', '-w', '--stdin'], content)
      git(['update-index', '--add', '--cacheinfo', '100644', blob, path])
    }
    const tree = git(['write-tree'])
    const baseline = git(['commit-tree', tree, '-m', `Frozen Device task tree ${tree}`])
    const ref = `refs/winwincode/task-baselines/${tree}`
    git(['update-ref', ref, baseline])
    return { baseline, tree, ref, taskInputDigest: digest }
  } finally {
    rmSync(index, { force: true })
    rmSync(`${index}.lock`, { force: true })
  }
}

const taskRepositories = new WeakMap()

export async function ensureBenchmarkDeviceOccupancy(context) {
  const occupancyPath = `/api/v1/clients/${context.devicePath.publicClientId}/occupancy`
  const rejected = code => { throw Object.assign(new Error(code), { code, benchmarkPhase: 'occupancy' }) }
  const deadline = performance.now() + 300_000
  const request = (path, options = {}) => context.api.request(path, {
    ...options, timeoutMillis: Math.max(1, deadline - performance.now()),
  })
  await waitFor(async () => {
    let occupancy = (await request(occupancyPath)).json
    if (occupancy.occupancy === 'available') {
      await request('/api/v1/clients/occupancy', { method: 'POST',
        body: { schemaVersion: 'winwincode/v1', clientId: context.devicePath.publicClientId } })
      occupancy = (await request(occupancyPath)).json
    }
    if (['reserving', 'recovery_pending'].includes(occupancy.occupancy)) {
      if (occupancy.holderUserId && occupancy.holderUserId !== context.api.actor.id) rejected('DEVICE_OCCUPANCY_FOREIGN_HOLDER')
      return false
    }
    if (occupancy.occupancy !== 'occupied') rejected('DEVICE_OCCUPANCY_NOT_READY')
    if (occupancy.holderUserId !== context.api.actor.id) rejected('DEVICE_OCCUPANCY_FOREIGN_HOLDER')
    return true
  }, 'Device occupancy recovery', 300_000, 1_000).catch(error => {
    if (error.code) throw error
    rejected('DEVICE_OCCUPANCY_RECOVERY_TIMEOUT')
  })
}

// Profile Devices keep their settings and databases separate, but this benchmark
// consumes the same four Provider accounts. Fixed hard links make their existing
// OS-lock admission share three slots per Provider across every profile.
export function prepareDeviceBenchmarkProviderSlots({ directory, profiles, providers }) {
  const privateDirectory = path => {
    mkdirSync(path, { recursive: true, mode: 0o700 })
    const metadata = lstatSync(path)
    assert.ok(metadata.isDirectory() && (metadata.mode & 0o777) === 0o700,
      `benchmark Provider slot directory must be private: ${path}`)
  }
  const privateSlot = path => {
    const metadata = lstatSync(path)
    assert.ok(metadata.isFile() && (metadata.mode & 0o777) === 0o600,
      `benchmark Provider slot must be a private regular file: ${path}`)
    return metadata
  }
  const shared = join(directory, 'model-provider-slots')
  privateDirectory(shared)
  for (const profile of profiles) assert.match(deviceBenchmarkProfileKey(profile), /^[a-zA-Z0-9][a-zA-Z0-9_-]*$/u,
    'benchmark configuration must name one Device directory')
  // The first profile owns device-data, while other profiles have named paths.
  // Admission may expand, but cannot reassign that retained Device to a different
  // immutable configuration on restart.
  const baseProfilePath = join(directory, 'base-profile.json')
  const baseProfile = `${JSON.stringify({ schemaVersion: 1,
    configurationId: profiles[0].configurationId, policyKey: deviceBenchmarkProfileKey(profiles[0]) }, null, 2)}\n`
  if (!existsSync(baseProfilePath) && existsSync(join(directory, 'device-data', 'device-client.sqlite3'))) {
    throw Object.assign(new Error('retained benchmark base Device has no profile identity'), {
      code: 'BENCHMARK_BASE_PROFILE_IDENTITY_MISSING',
    })
  }
  try { writeFileSync(baseProfilePath, baseProfile, { flag: 'wx', mode: 0o600 }) }
  catch (error) { if (error.code !== 'EEXIST') throw error }
  privateSlot(baseProfilePath)
  if (readFileSync(baseProfilePath, 'utf8') !== baseProfile) {
    throw Object.assign(new Error('retained benchmark base Device belongs to another profile'), {
      code: 'BENCHMARK_BASE_PROFILE_MISMATCH',
    })
  }
  const devices = profiles.map((profile, index) => {
    const device = index === 0 ? join(directory, 'device-data')
      : join(directory, 'devices', deviceBenchmarkProfileKey(profile))
    privateDirectory(device)
    const providerDirectory = join(device, 'providers')
    privateDirectory(providerDirectory)
    const slots = join(providerDirectory, 'model-provider-slots')
    privateDirectory(slots)
    return slots
  })
  for (const { providerId } of providers) {
    assert.ok(typeof providerId === 'string' && providerId.trim() === providerId && providerId.length > 0,
      'benchmark Provider must have an exact identity')
    const digest = createHash('sha256').update(providerId).digest('hex')
    const providerShared = join(shared, digest)
    privateDirectory(providerShared)
    const providerDevices = devices.map(device => {
      const path = join(device, digest)
      privateDirectory(path)
      return path
    })
    for (let index = 0; index < 3; index++) {
      const source = join(providerShared, `slot-${index}`)
      try { closeSync(openSync(source, 'wx', 0o600)) }
      catch (error) { if (error.code !== 'EEXIST') throw error }
      const expected = privateSlot(source)
      for (const device of providerDevices) {
        const destination = join(device, `slot-${index}`)
        try { linkSync(source, destination) }
        catch (error) { if (error.code !== 'EEXIST') throw error }
        const actual = privateSlot(destination)
        assert.ok(actual.dev === expected.dev && actual.ino === expected.ino,
          `benchmark Device Provider slot has an independent inode: ${destination}`)
      }
    }
  }
}

// Delivery creation checks the registered Device repository HEAD. Register a
// detached baseline once per Device; concurrent Sessions get separate execution
// worktrees from the product, while identical baselines share its unique binding.
export async function registerDeviceTaskRepository(runtime, taskInputPath, directory) {
  const prepared = prepareDeviceTaskBaseline(runtime.repository, taskInputPath, join(directory, 'git-index'))
  let repositories = taskRepositories.get(runtime.devicePath)
  if (!repositories) { repositories = new Map(); taskRepositories.set(runtime.devicePath, repositories) }
  if (!repositories.has(prepared.baseline)) repositories.set(prepared.baseline,
    registerTaskRepository(runtime, prepared, directory))
  return { ...runtime, ...await repositories.get(prepared.baseline) }
}

async function registerTaskRepository(runtime, prepared, directory) {
  const git = (repository, args) => execFileSync('git', ['-C', repository, ...args], { encoding: 'utf8' }).trim()
  const common = realpathSync(git(runtime.repository, ['rev-parse', '--path-format=absolute', '--git-common-dir']))
  const bindings = checkedWwc(runtime.devicePath.wwc, ['repo', 'list',
    '--data-dir', runtime.devicePath.deviceData, '--json'], runtime.devicePath.deviceEnvironment).repositories
  const retained = bindings.find(binding => binding.headCommit === prepared.baseline
    && binding.gitCommonDirectory === common)
  const repository = retained?.canonicalPath ?? join(directory, 'source')
  if (!existsSync(repository)) {
    assert.equal(retained, undefined, 'retained task worktree is missing')
    execFileSync('git', ['-C', runtime.repository, 'worktree', 'add', '--quiet', '--detach',
      repository, prepared.baseline], { stdio: ['ignore', 'pipe', 'pipe'] })
  }
  assert.equal(realpathSync(git(repository, ['rev-parse', '--path-format=absolute', '--git-common-dir'])), common,
    'retained task worktree belongs to another repository')
  assert.equal(git(repository, ['rev-parse', 'HEAD']), prepared.baseline, 'retained task baseline changed')
  assert.equal(git(repository, ['status', '--porcelain']), '', 'retained task worktree is dirty')
  const registered = registerOrReuseDeviceRepository({ ...runtime.devicePath, repository })
  const repositoryBindingId = registered.repositoryBindingId
  await waitFor(async () => {
    const response = await runtime.api.request(`/api/v1/repositories?clientId=${runtime.devicePath.publicClientId}`)
    return response.status === 200 && response.json.repositories.some(item => item.repositoryBindingId === repositoryBindingId)
  }, 'task repository binding projection', 60_000)
  return { repository, baseline: prepared.baseline, repositoryBindingId }
}

// Configuration profiles live on separate Device clients because their Worker
// settings are immutable process inputs. Tasks share the Server and their
// profile's Device; only product Session/Delivery identities vary per launch.
export async function withDeviceTaskRuntime({ directory, profiles, providers, agentSettings,
  providerEnvironment = process.env, automaticTaskActions = false, build = false, timeoutMillis }, run) {
  assert.ok(profiles.length > 0 && providers.length > 0)
  const first = profiles[0], primary = providers[0]
  const additionalDevices = []
  prepareDeviceBenchmarkProviderSlots({ directory, profiles, providers })
  return runApiProductionVertical({ directory, build, restart: false, repeat: false,
    ...(timeoutMillis === undefined ? {} : { timeoutMillis }),
    retainRepository: true, deviceProvider: primary,
    deviceProviderSecrets: providers.flatMap(deviceProviderSecretValues).filter(Boolean),
    deviceRoute: { providerId: primary.providerId, modelId: primary.modelId },
    deviceAgentEnvironment: benchmarkDeviceEnvironment(first, { ...agentSettings, automaticTaskActions }, providerEnvironment),
    serverEnvironment: { WWC_SERVER_WORKER_MODE: 'remote', WWC_SERVER_MAX_RUNTIME_SECONDS: 'unlimited',
      WWC_SERVER_EXECUTION_LEASE_SECONDS: '900' },
    scenario: { async run(base) {
      const contexts = new Map()
      try {
        for (const profile of profiles) {
          const expectedAgentEnvironment = benchmarkDeviceEnvironment(profile,
            { ...agentSettings, automaticTaskActions }, providerEnvironment)
          const devicePath = profile === first ? base.devicePath : await establishDeviceOnlyExecutionPath({
            api: base.api, wwc: base.devicePath.wwc, directory, repository: base.repository,
            deviceData: join(directory, 'devices', deviceBenchmarkProfileKey(profile)),
            logName: `device-${deviceBenchmarkProfileKey(profile)}.log`,
            schemaVersion: base.devicePath.schemaVersion,
            helperReleaseManifest: base.devicePath.deviceEnvironment.WWC_WORKER_HELPER_RELEASE_MANIFEST,
            helperExecutable: base.devicePath.deviceEnvironment.WWC_WORKER_HELPER_EXECUTABLE,
            modelRouteSource: primary, deviceProvider: primary, deviceSecret: primary.apiKey,
            repositoryScope: { organizationId: 'org_01J00000000000000000000000',
              workspaceId: 'wsp_01J00000000000000000000000', projectId: 'prj_01J00000000000000000000000',
              repositoryId: 'rep_01J00000000000000000000000' },
            agentEnvironment: expectedAgentEnvironment,
          })
          if (profile !== first) additionalDevices.push(devicePath)
          assertDeviceAgentEnvironmentForwarded(expectedAgentEnvironment, devicePath.deviceEnvironment)
          for (const provider of providers.slice(1)) await seedDeviceLocalProvider({
            api: base.api, publicClientId: devicePath.publicClientId, ...provider,
          })
          contexts.set(deviceBenchmarkProfileKey(profile), { ...base, directory, devicePath })
        }
        return await run({ async forTask(request, prepared) {
          const context = contexts.get(deviceBenchmarkProfileKey(request))
          assert.ok(context, 'task configuration was not provisioned')
          const provider = providers.find(provider => provider.modelId === request.provider)
          assert.ok(provider, 'task model was not provisioned')
          await ensureBenchmarkDeviceOccupancy(context)
          const bound = await registerDeviceTaskRepository(context, prepared.taskInputPath,
            join(prepared.directory, 'repository'))
          return { ...bound, modelRoute: configuredDeviceModelRoute({
            clientNodeId: context.devicePath.seededProvider.clientNodeId,
            providerId: provider.providerId, modelId: provider.modelId,
          }) }
        } })
      } finally {
        const stopped = await Promise.allSettled(additionalDevices.map(device => device.stop()))
        for (const result of stopped) if (result.status === 'rejected') throw result.reason
      }
    } },
  })
}
