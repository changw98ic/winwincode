#!/usr/bin/env node

import assert from 'node:assert/strict'
import { spawn, spawnSync } from 'node:child_process'
import {
  chmodSync,
  cpSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, dirname, join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

import {
  NATIVE_TARGETS,
  hostNativeTarget,
  nativeTargetConfiguration,
  verifyNativePrebuild,
} from './native-package-contract.mjs'

const COMMAND_TIMEOUT_MILLIS = 600_000
const WEB_TIMEOUT_MILLIS = 45_000
const TERMINATION_GRACE_MILLIS = 5_000

function parseArguments(argv) {
  let target
  let requireRelease = false
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    if (argument === '--require-release') {
      requireRelease = true
      continue
    }
    if (argument === '--target') {
      target = argv[index + 1]
      if (target === undefined) throw new Error('--target requires a Rust target triple')
      index += 1
      continue
    }
    if (argument.startsWith('--target=')) {
      target = argument.slice('--target='.length)
      continue
    }
    throw new Error(`unknown verify-installed-host argument: ${argument}`)
  }
  return { target: target ?? hostNativeTarget(), requireRelease }
}

function run(command, arguments_, options = {}) {
  const result = spawnSync(command, arguments_, {
    encoding: 'utf8',
    maxBuffer: 64 * 1_024 * 1_024,
    timeout: COMMAND_TIMEOUT_MILLIS,
    ...options,
  })
  if (result.error !== undefined) throw result.error
  if (result.signal !== null || result.status !== 0) {
    throw new Error(
      `${command} ${arguments_.join(' ')} failed with `
      + `${result.signal ?? String(result.status)}\n${result.stdout}${result.stderr}`,
    )
  }
  return result.stdout
}

function pack(root, directory, destination) {
  const output = run('corepack', [
    'pnpm',
    '--dir',
    join(root, directory),
    'pack',
    '--json',
    '--pack-destination',
    destination,
  ], { cwd: root })
  const report = JSON.parse(output)
  const filename = report.filename ?? report[0]?.filename
  if (typeof filename !== 'string') throw new Error(`${directory}: pnpm pack reported no file`)
  return resolve(join(root, directory), filename)
}

function extractPackedPackage(tarball, target, stagingRoot, replace = true) {
  const staging = mkdtempSync(join(stagingRoot, 'package-'))
  try {
    run('tar', ['-xzf', tarball, '-C', staging])
    const source = join(staging, 'package')
    const actualTarget = replace ? realpathSync(target) : target
    if (replace) rmSync(actualTarget, { recursive: true })
    mkdirSync(actualTarget, { recursive: true })
    cpSync(source, actualTarget, { recursive: true, force: true })
    return actualTarget
  } finally {
    rmSync(staging, { recursive: true, force: true })
  }
}

function platformPackageDirectory(installation, packageName) {
  const namespaceName = packageName.slice('@winwincode/'.length)
  const virtualStore = join(installation, 'node_modules', '.pnpm')
  const prefix = `${packageName.replace('/', '+')}@file+packages+${namespaceName}`
  const matches = readdirSync(virtualStore).filter(name => name.startsWith(prefix))
  if (matches.length !== 1) {
    throw new Error(`portable install has ${String(matches.length)} ${packageName} package roots`)
  }
  return join(
    virtualStore,
    matches[0],
    'node_modules',
    '@winwincode',
    namespaceName,
  )
}

function removeBrokenWorkspaceLinks(installation, currentPackageName) {
  const flatHost = join(installation, 'node_modules', '.pnpm', 'node_modules', 'winwincode')
  if (lstatOrUndefined(flatHost)?.isSymbolicLink() === true) unlinkSync(flatHost)
  const nativeRoot = realpathSync(join(installation, 'node_modules', '@winwincode', 'native'))
  const namespace = join(dirname(dirname(nativeRoot)), '@winwincode')
  for (const name of readdirSync(namespace)) {
    if (!name.startsWith('native-') || `@winwincode/${name}` === currentPackageName) continue
    const path = join(namespace, name)
    if (lstatOrUndefined(path)?.isSymbolicLink() === true) unlinkSync(path)
  }
}

function lstatOrUndefined(path) {
  try {
    return lstatSync(path)
  } catch (error) {
    if (error?.code === 'ENOENT') return undefined
    throw error
  }
}

function keylessEnvironment(extra) {
  return {
    ...Object.fromEntries(Object.entries(process.env).filter(([name]) => (
      !/(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/u.test(name)
    ))),
    ...extra,
  }
}

function jsonLine(value) {
  const line = value.trim().split(/\r?\n/u).findLast(entry => entry.trim().startsWith('{'))
  if (line === undefined) throw new Error(`process emitted no JSON response:\n${value}`)
  return JSON.parse(line)
}

function runCli(cli, args, environment, workspace, expectedStatus) {
  const result = spawnSync(cli, args, {
    cwd: workspace,
    encoding: 'utf8',
    env: environment,
    timeout: 45_000,
  })
  if (result.error !== undefined) throw result.error
  assert.equal(result.signal, null, `${args.join(' ')} ended with ${result.signal}`)
  assert.equal(result.status, expectedStatus, result.stderr || result.stdout)
  return jsonLine(expectedStatus === 0 ? result.stdout : result.stderr)
}

function terminateProcessGroup(child, signal) {
  if (child.pid === undefined) return
  try {
    process.kill(-child.pid, signal)
  } catch (error) {
    if (error?.code !== 'ESRCH') child.kill(signal)
  }
}

async function startAndInspectWeb(cli, environment, workspace, runningChildren) {
  const child = spawn(cli, ['web', '--no-open', '--port', '0'], {
    cwd: workspace,
    detached: true,
    env: environment,
    stdio: ['ignore', 'pipe', 'pipe'],
  })
  runningChildren.add(child)
  child.stdout.setEncoding('utf8')
  child.stderr.setEncoding('utf8')
  let stdout = ''
  let stderr = ''
  let resolveUrl
  const urlReady = new Promise(resolvePromise => { resolveUrl = resolvePromise })
  child.stdout.on('data', chunk => {
    stdout += chunk
    const match = stdout.match(/dsh web:\s+(https?:\/\/\S+)/u)
    if (match?.[1] !== undefined) resolveUrl(match[1])
  })
  child.stderr.on('data', chunk => { stderr += chunk })
  const exited = new Promise(resolvePromise => {
    child.once('exit', (code, signal) => resolvePromise({ code, signal }))
  })
  const timeout = new Promise((_, reject) => {
    setTimeout(() => reject(new Error(`installed Web startup timed out\n${stderr}`)), WEB_TIMEOUT_MILLIS)
      .unref()
  })
  try {
    const url = await Promise.race([
      urlReady,
      exited.then(result => {
        throw new Error(`installed Web exited before URL: ${JSON.stringify(result)}\n${stderr}`)
      }),
      timeout,
    ])
    const response = await fetch(url)
    assert.equal(response.status, 200)
    const html = await response.text()
    assert.match(html, /window\.__DSH_BOOT__/u)
    assert.match(html, /@deepseek-ai\/dsh-client-ui-conversation/u)
    assert.match(html, /@winwincode\/strongflow/u)
    const clientPath = html.match(/"id":"@winwincode\/strongflow","url":"([^"]+)"/u)?.[1]
    assert.notEqual(clientPath, undefined)
    const clientResponse = await fetch(new URL(clientPath, url))
    assert.equal(clientResponse.status, 200)
    assert.match(await clientResponse.text(), /@winwincode\/strongflow/u)
    child.kill('SIGTERM')
    const result = await Promise.race([
      exited,
      new Promise(resolvePromise => {
        setTimeout(() => resolvePromise(null), TERMINATION_GRACE_MILLIS).unref()
      }),
    ])
    if (result === null) {
      terminateProcessGroup(child, 'SIGKILL')
      throw new Error('installed Web ignored SIGTERM')
    }
    assert.deepEqual(result, { code: 143, signal: null })
    return { html, stdout }
  } finally {
    runningChildren.delete(child)
    if (child.exitCode === null && child.signalCode === null) terminateProcessGroup(child, 'SIGKILL')
  }
}

function treeContainsBytes(root, wanted) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const path = join(root, entry.name)
    if (entry.isSymbolicLink()) continue
    if (entry.isDirectory()) {
      if (treeContainsBytes(path, wanted)) return true
    } else if (entry.isFile() && readFileSync(path).includes(wanted)) {
      return true
    }
  }
  return false
}

const root = resolve(import.meta.dirname, '..')
const { target, requireRelease } = parseArguments(process.argv.slice(2))
if (target === undefined) {
  throw new Error(
    `unsupported host ${process.platform}/${process.arch}; expected `
    + NATIVE_TARGETS.map(configuration => configuration.host).join(', '),
  )
}
const configuration = nativeTargetConfiguration(target)
if (configuration === undefined) throw new Error(`unsupported native target ${target}`)
const prebuild = verifyNativePrebuild({
  root,
  target,
  requireRelease,
  requireCurrentHost: true,
})
if (prebuild.errors.length > 0) throw new Error(prebuild.errors.join('\n'))

const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-installed-host-'))
const tarballRoot = join(temporaryRoot, 'tarballs')
const stagingRoot = join(temporaryRoot, 'staging')
const installation = join(temporaryRoot, 'installation')
const dshHome = join(temporaryRoot, 'dsh-home')
const workspace = join(temporaryRoot, 'workspace')
const runningChildren = new Set()
mkdirSync(tarballRoot)
mkdirSync(stagingRoot)
mkdirSync(workspace)

try {
  const packageTarballs = new Map([
    ['host', pack(root, 'apps/host', tarballRoot)],
    ['contracts', pack(root, 'packages/contracts', tarballRoot)],
    ['dsh-profile', pack(root, 'packages/dsh-profile', tarballRoot)],
    ['native', pack(root, 'packages/native', tarballRoot)],
    ['strongflow', pack(root, 'packages/strongflow', tarballRoot)],
    ['platform', pack(root, configuration.packageDirectory, tarballRoot)],
  ])
  run('corepack', [
    'pnpm',
    '--filter',
    'winwincode',
    'deploy',
    '--legacy',
    installation,
  ], { cwd: root })

  const packageTargets = {
    contracts: join(installation, 'node_modules', '@winwincode', 'contracts'),
    'dsh-profile': join(installation, 'node_modules', '@winwincode', 'dsh-profile'),
    native: join(installation, 'node_modules', '@winwincode', 'native'),
    strongflow: join(installation, 'node_modules', '@winwincode', 'strongflow'),
    platform: platformPackageDirectory(installation, configuration.packageName),
  }
  for (const [name, targetPath] of Object.entries(packageTargets)) {
    packageTargets[name] = extractPackedPackage(
      packageTarballs.get(name),
      targetPath,
      stagingRoot,
    )
  }
  extractPackedPackage(packageTarballs.get('host'), installation, stagingRoot, false)
  removeBrokenWorkspaceLinks(installation, configuration.packageName)

  const hostManifest = JSON.parse(readFileSync(join(installation, 'package.json'), 'utf8'))
  assert.equal(hostManifest.dependencies['@deepseek-ai/dsh'], '0.1.0-rc.8')
  assert.ok(Object.values(hostManifest.dependencies).every(value => !value.startsWith('workspace:')))
  const cli = join(installation, 'dist', 'cli.js')
  assert.notEqual(statSync(cli).mode & 0o111, 0)
  chmodSync(cli, statSync(cli).mode | 0o755)

  const keylessFixture = join(packageTargets['dsh-profile'], 'installed-host-keyless.mjs')
  const planReviewFixture = join(packageTargets.strongflow, 'installed-host-plan-review.mjs')
  cpSync(join(root, 'tests', 'fixtures', 'installed-host-keyless.mjs'), keylessFixture)
  cpSync(join(root, 'tests', 'fixtures', 'installed-host-plan-review.mjs'), planReviewFixture)
  const environment = keylessEnvironment({
    CI: '1',
    DSH_HOME: dshHome,
    DSH_TELEMETRY_DISABLED: '1',
    WINWINCODE_CLI_AUTH_PROOF: 'installed-cli-proof-value',
  })

  const version = run(cli, ['--version'], { cwd: workspace, env: environment }).trim()
  assert.equal(version, hostManifest.version)
  const scaffold = JSON.parse(run(cli, ['--print-scaffold'], {
    cwd: workspace,
    env: environment,
  }))
  assert.equal(scaffold.defaultSurface.id, 'chat')
  assert.equal(scaffold.defaultSurface.default, true)
  assert.equal(scaffold.surfaces.find(surface => surface.id === 'strongflow').default, false)

  const keyless = jsonLine(run(process.execPath, [keylessFixture], {
    cwd: packageTargets['dsh-profile'],
    env: {
      ...environment,
      WINWINCODE_SMOKE_HOME: join(dshHome, 'winwincode'),
      WINWINCODE_SMOKE_WORKSPACE: workspace,
    },
  }))
  assert.deepEqual(keyless.surfaces.map(surface => [surface.id, surface.default]), [
    ['chat', true],
    ['strongflow', false],
  ])
  assert.equal(keyless.kernelCreations, 1)
  assert.equal(keyless.nativeTarget, target)
  assert.equal(new Set(keyless.kernelSessionIds).size, 2)
  assert.deepEqual(keyless.roles, ['chat', 'requirements'])
  assert.deepEqual(keyless.credentialEnvironment, [])
  assert.ok(keyless.eventKinds.every(kinds => (
    kinds.includes('turn.started') && kinds.includes('turn.completed')
  )))
  const installedRoot = realpathSync(temporaryRoot)
  assert.ok(keyless.modulePaths.every(path => realpathSync(path).startsWith(installedRoot)))
  assert.deepEqual(keyless.shutdown, { completed: [], submitFailed: [], timedOut: [] })

  const contracts = await import(pathToFileURL(
    join(packageTargets.contracts, 'dist', 'index.js'),
  ).href)
  const initialTime = Date.now() - 5_000
  const deliveryId = contracts.generateDeliveryId(initialTime)
  const spec = revision => ({
    schemaVersion: contracts.DELIVERY_SCHEMA_VERSION,
    id: `${deliveryId}:spec:${String(revision)}`,
    deliveryId,
    revision,
    title: `Installed Delivery revision ${String(revision)}`,
    goal: 'Prove the published CLI and DSH Web host share one durable Delivery.',
    scope: ['Installed package process boundary'],
    outOfScope: ['Generic task management'],
    constraints: ['Codex Core remains the execution authority'],
    acceptanceCriteria: [{
      schemaVersion: contracts.DELIVERY_SCHEMA_VERSION,
      id: `${deliveryId}:criterion:${String(revision)}`,
      description: 'The installed process survives review and restart.',
      verificationMethod: 'Run this installed-package process smoke.',
      required: true,
    }],
    sourceRef: null,
    publicationTarget: null,
    repository: {
      schemaVersion: contracts.DELIVERY_SCHEMA_VERSION,
      kind: 'local-git',
      locator: workspace,
    },
    baseRevision: '1'.repeat(40),
    maxReworkAttempts: 2,
    createdAtMillis: initialTime + revision,
  })
  const initialTask = [{
    schemaVersion: contracts.DELIVERY_SCHEMA_VERSION,
    id: `${deliveryId}:task:host`,
    deliveryId,
    title: 'Installed host process',
    goal: 'Exercise one independently reviewable installed process.',
    acceptanceCriterionIds: [`${deliveryId}:criterion:1`],
    blockedByTaskIds: [],
    owner: 'installed-owner',
    status: 'pending',
  }]
  const specOnePath = join(temporaryRoot, 'spec-1.json')
  const specTwoPath = join(temporaryRoot, 'spec-2.json')
  const tasksPath = join(temporaryRoot, 'tasks.json')
  writeFileSync(specOnePath, `${JSON.stringify(spec(1))}\n`)
  writeFileSync(specTwoPath, `${JSON.stringify(spec(2))}\n`)
  writeFileSync(tasksPath, `${JSON.stringify(initialTask)}\n`)

  const created = runCli(cli, [
    'delivery', 'create', '--spec', specOnePath, '--tasks', tasksPath,
    '--request-id', 'installed-create', '--json',
  ], environment, workspace, 0)
  assert.equal(created.result.delivery.status, 'draft')
  const reopened = runCli(cli, [
    'delivery', 'show', deliveryId, '--request-id', 'installed-show-created', '--json',
  ], environment, workspace, 0)
  assert.deepEqual(reopened.result.delivery, created.result.delivery)

  const clarifying = runCli(cli, [
    'delivery', 'start-stage', deliveryId, '--expected-revision', '1',
    '--stage-run-id', 'stage-installed-clarifying', '--stage', 'clarifying',
    '--actor', 'codex', '--role', 'requirements', '--request-id', 'installed-clarify', '--json',
  ], environment, workspace, 0)
  assert.equal(clarifying.result.delivery.status, 'clarifying')
  runCli(cli, [
    'delivery', 'bind-session', deliveryId, '--expected-revision', '2',
    '--binding-id', 'binding-installed-clarifying',
    '--stage-run-id', 'stage-installed-clarifying',
    '--dsh-session', 'dsh-installed-clarifying',
    '--codex-session', 'codex-installed-clarifying',
    '--request-id', 'installed-bind-clarifying', '--json',
  ], environment, workspace, 0)
  const stale = runCli(cli, [
    'delivery', 'update-spec', deliveryId, '--expected-revision', '2',
    '--spec', specTwoPath, '--request-id', 'installed-stale-spec', '--json',
  ], environment, workspace, 4)
  assert.equal(stale.error.code, 'REVISION_CONFLICT')
  assert.equal(stale.error.currentRevision, 3)
  const ready = runCli(cli, [
    'delivery', 'update-spec', deliveryId, '--expected-revision', '3',
    '--spec', specTwoPath, '--request-id', 'installed-current-spec', '--json',
  ], environment, workspace, 0)
  assert.equal(ready.result.delivery.status, 'ready')
  runCli(cli, [
    'delivery', 'start-stage', deliveryId, '--expected-revision', '4',
    '--stage-run-id', 'stage-installed-planning', '--stage', 'planning',
    '--actor', 'codex', '--role', 'planner', '--request-id', 'installed-plan', '--json',
  ], environment, workspace, 0)
  runCli(cli, [
    'delivery', 'bind-session', deliveryId, '--expected-revision', '5',
    '--binding-id', 'binding-installed-planning', '--stage-run-id', 'stage-installed-planning',
    '--dsh-session', 'dsh-installed-planning', '--codex-session', 'codex-installed-planning',
    '--request-id', 'installed-bind-planning', '--json',
  ], environment, workspace, 0)

  const planningPath = join(temporaryRoot, 'planning.json')
  const planning = runCli(cli, [
    'delivery', 'show', deliveryId, '--request-id', 'installed-show-planning', '--json',
  ], environment, workspace, 0)
  writeFileSync(planningPath, `${JSON.stringify(planning)}\n`)
  const reviewValues = jsonLine(run(process.execPath, [planReviewFixture, planningPath], {
    cwd: packageTargets.strongflow,
    env: environment,
  }))
  const attentionPath = join(temporaryRoot, 'attention.json')
  writeFileSync(attentionPath, `${JSON.stringify(reviewValues.attention)}\n`)
  runCli(cli, [
    'delivery', 'start-stage', deliveryId, '--expected-revision', '6',
    '--stage-run-id', 'stage-installed-plan-review', '--stage', 'plan-review',
    '--actor', 'human', '--role', 'reviewer', '--attention', attentionPath,
    '--request-id', 'installed-open-review', '--json',
  ], environment, workspace, 0)
  runCli(cli, [
    'delivery', 'bind-session', deliveryId, '--expected-revision', '7',
    '--binding-id', 'binding-installed-plan-review',
    '--stage-run-id', 'stage-installed-plan-review',
    '--dsh-session', 'dsh-installed-plan-review',
    '--request-id', 'installed-bind-review', '--json',
  ], environment, workspace, 0)
  const waiting = runCli(cli, [
    'delivery', 'show', deliveryId, '--request-id', 'installed-show-attention', '--json',
  ], environment, workspace, 0)
  assert.equal(waiting.result.delivery.status, 'needs-attention')
  assert.equal(waiting.result.delivery.attentionItems.at(-1).status, 'open')

  const firstWeb = await startAndInspectWeb(cli, environment, workspace, runningChildren)
  const afterInterruption = runCli(cli, [
    'delivery', 'show', deliveryId, '--request-id', 'installed-show-after-signal', '--json',
  ], environment, workspace, 0)
  assert.equal(afterInterruption.result.delivery.attentionItems.at(-1).status, 'open')
  const secondWeb = await startAndInspectWeb(cli, environment, workspace, runningChildren)
  assert.equal(
    /@winwincode\/strongflow/u.test(firstWeb.html),
    /@winwincode\/strongflow/u.test(secondWeb.html),
  )

  const resolved = runCli(cli, [
    'delivery', 'resolve-attention', deliveryId, '--expected-revision', '8',
    '--attention-id', reviewValues.attention.id, '--decision', 'resolved',
    '--resolution', JSON.stringify(reviewValues.decision),
    '--auth', environment.WINWINCODE_CLI_AUTH_PROOF,
    '--request-id', 'installed-resolve-review', '--json',
  ], environment, workspace, 0)
  assert.equal(resolved.result.delivery.status, 'executing')
  assert.equal(resolved.result.delivery.attentionItems.at(-1).status, 'resolved')
  const final = runCli(cli, [
    'delivery', 'show', deliveryId, '--request-id', 'installed-show-resolved', '--json',
  ], environment, workspace, 0)
  assert.deepEqual(final.result.delivery, resolved.result.delivery)
  assert.equal(
    treeContainsBytes(dshHome, Buffer.from(environment.WINWINCODE_CLI_AUTH_PROOF, 'utf8')),
    false,
  )

  process.stdout.write(
    `installed host package passed DSH Web, keyless chat, CLI, signal, restart, `
    + `Attention, and cleanup smokes for ${target}\n`,
  )
} finally {
  for (const child of runningChildren) terminateProcessGroup(child, 'SIGKILL')
  rmSync(temporaryRoot, { recursive: true, force: true })
}
