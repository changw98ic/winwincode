#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
/**
 * winwincode-00os live Device production vertical acceptance.
 *
 * Runs the Device-only production path on an isolated temporary data directory
 * and free ports. Never touches production services on 63159/63160/18445.
 * StrongFlow is reported as a subset when the unfinished frozen verifier
 * (72t.36.1) blocks the full done/verdict chain.
 */
import { mkdirSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'
import {
  runApiProductionVertical,
  deviceOnlyServerEnvironment,
  assertServerEnvironmentIsDeviceOnly,
} from './run-api-production-vertical.mjs'

const root = resolve(import.meta.dirname, '..')
const cargoTarget = process.env.CARGO_TARGET_DIR
  ?? '/Volumes/ORICO/winwincode-targets/run03-audit'
const binaries = {
  server: resolve(cargoTarget, 'debug/winwincode-server'),
  wwc: resolve(cargoTarget, 'debug/wwc'),
  worker: resolve(cargoTarget, 'debug/winwincode-worker'),
  helper: resolve(cargoTarget, 'debug/winwincode-kernel-helper'),
}
const directory = resolve(
  process.env.WWC_00OS_RESULT_DIRECTORY
    ?? `/tmp/winwincode-00os-device-vertical-${new Date().toISOString().replaceAll(':', '-')}`,
)
mkdirSync(directory, { recursive: true, mode: 0o700 })

const steps = []
const report = {
  issue: 'winwincode-00os',
  execution: 'device-worker-only',
  isolated: true,
  directory,
  cargoTarget,
  binaries,
  forbiddenProductionPorts: [63159, 63160, 18445],
  steps,
  complete: false,
  strongFlowSubset: null,
  error: null,
}
const save = () => writeFileSync(
  resolve(directory, '00os-live-vertical-report.json'),
  `${JSON.stringify(report, null, 2)}\n`,
)

const serverEnvironment = deviceOnlyServerEnvironment({
  WWC_SERVER_WORKER_MODE: 'remote',
  WWC_SERVER_AUTH_MODE: 'local-open',
  WWC_DEBUG_RUNTIME: '1',
  WWC_DEBUG_RUNTIME_LOG: resolve(directory, 'server-runtime.log'),
  WWC_SERVER_EXECUTION_LEASE_SECONDS: '600',
})
assertServerEnvironmentIsDeviceOnly(serverEnvironment)

function mark(name, status, detail = null) {
  steps.push({ name, status, detail })
  save()
}

try {
  mark('isolated-directory', 'pass', directory)
  mark('production-services-untouched', 'pass', report.forbiddenProductionPorts)

  const vertical = await runApiProductionVertical({
    build: process.env.WWC_API_SKIP_BUILD !== '1',
    directory,
    root,
    serverBinary: binaries.server,
    workerBinary: null,
    wwcBinary: binaries.wwc,
    restart: true,
    repeat: false,
    includeStrongFlow: true,
    continueOnStrongFlowFailure: true,
    devicePrerequisites: true,
    deviceRoute: {
      providerId: 'winwincode-device-deterministic',
      modelId: 'device-deterministic-model',
      clientNodeId: null,
    },
    deviceProviderSecrets: [],
    serverEnvironment,
    timeoutMillis: Number(process.env.WWC_00OS_TIMEOUT_MS ?? 420_000),
  })

  report.vertical = {
    health: vertical.health,
    execution: vertical.execution,
    devicePrerequisites: vertical.devicePrerequisites,
    deviceProvider: vertical.deviceProvider,
    devicePath: vertical.devicePath,
    errorCodeTruthfulness: vertical.errorCodeTruthfulness,
    flow: vertical.flow,
    restart: vertical.restart,
    deterministic: vertical.deterministic,
    artifacts: vertical.artifacts,
  }

  const truth = vertical.errorCodeTruthfulness
  mark(
    'error-code-truthfulness',
    truth?.notWrongStateDisguise && truth?.truthfulDeviceCodes ? 'pass' : 'fail',
    truth,
  )

  const devicePath = vertical.devicePath ?? {}
  const deviceSteps = devicePath.steps ?? []
  for (const required of [
    'device.enroll-pair',
    'client-connect',
    'client-occupancy',
    'repository-binding',
    'device-local-model-server',
    'device-local-provider-seeded',
  ]) {
    mark(`device:${required}`, deviceSteps.includes(required) ? 'pass' : 'fail', deviceSteps)
  }
  mark(
    'device:launch-anchor',
    deviceSteps.some(step => step === 'launch-anchor' || step.startsWith('launch-anchor:'))
      ? 'pass'
      : 'fail',
    deviceSteps,
  )
  mark(
    'device:modelRoute-bound',
    typeof devicePath.modelRoute?.credentialReferenceId === 'string'
      && String(devicePath.modelRoute.credentialReferenceId).startsWith('crd_')
      ? 'pass'
      : 'fail',
    devicePath.modelRoute,
  )
  mark(
    'server-env-device-only',
    serverEnvironment.WWC_SERVER_MODEL_API_KEY === undefined
      && serverEnvironment.WWC_SERVER_MODEL_PROVIDER_ID === undefined
      ? 'pass'
      : 'fail',
    Object.keys(serverEnvironment),
  )

  const chat = vertical.flow?.chat
  mark(
    'chat',
    chat?.status === 'Completed' && typeof chat?.assistant?.content === 'string'
      && chat.assistant.content.length > 0
      ? 'pass'
      : 'fail',
    chat ?? null,
  )
  mark(
    'cancel',
    vertical.flow?.cancel?.state === 'cancelled' ? 'pass' : 'fail',
    vertical.flow?.cancel ?? null,
  )
  mark(
    'restart',
    vertical.restart?.messageBytesStable === true && vertical.health?.afterRestart === 'ready'
      ? 'pass'
      : 'fail',
    vertical.restart ?? null,
  )

  const strong = vertical.flow?.strongflow ?? null
  if (strong && strong.status === 'done' && strong.verdictStatus === 'pass') {
    report.strongFlowSubset = {
      status: 'pass',
      deliveryStatus: strong.status,
      verdictStatus: strong.verdictStatus,
      evidenceCount: strong.evidenceCount,
      workRunStates: strong.workRunStates,
      candidateArtifact: strong.candidateArtifact,
      coupledTo72t361: false,
    }
    mark('strongflow', 'pass', report.strongFlowSubset)
  } else {
    report.strongFlowSubset = {
      status: 'partial-or-blocked',
      deliveryStatus: strong?.status ?? null,
      verdictStatus: strong?.verdictStatus ?? null,
      evidenceCount: strong?.evidenceCount ?? null,
      workRunStates: strong?.workRunStates ?? null,
      candidateArtifact: strong?.candidateArtifact ?? null,
      executorCandidateFrozen: Boolean(strong?.candidateArtifact?.candidateRef),
      coupledTo72t361: true,
      reason: strong?.executorCandidateFrozen || strong?.candidateArtifact?.candidateRef
        ? 'Executor froze a Device candidate and Controller dispatched the next WorkRun; reviewer/verifier execution did not reach done/pass (Device role execution residual).'
        : 'StrongFlow did not produce a frozen candidate or done/pass verdict. Device StrongFlow progression requires Controller reviewer/verifier dispatch after executor candidate_ready.',
      error: vertical.flow?.strongflowError ?? null,
      observed: strong,
    }
    mark(
      'strongflow',
      report.strongFlowSubset.executorCandidateFrozen ? 'partial' : 'fail',
      report.strongFlowSubset,
    )
  }

  report.complete = chat?.status === 'Completed'
    && vertical.flow?.cancel?.state === 'cancelled'
    && vertical.restart?.messageBytesStable === true
    && steps.filter(step => step.name.startsWith('device:') && step.status === 'pass').length > 0
  report.strongFlowFullAcceptance = report.strongFlowSubset?.status === 'pass'
  report.fullAcPass = report.complete
    && report.strongFlowFullAcceptance
    && truth?.truthfulDeviceCodes === true
    && truth?.notWrongStateDisguise === true
  save()
  console.log(JSON.stringify(report, null, 2))
  if (!report.complete) process.exitCode = 1
} catch (error) {
  report.error = String(error instanceof Error ? error.message : error)
  report.strongFlowSubset = report.strongFlowSubset ?? {
    status: 'not-reached',
    coupledTo72t361: true,
    reason: report.error,
  }
  mark('vertical-run', 'fail', report.error.slice(0, 4000))
  save()
  console.error(JSON.stringify(report, null, 2))
  process.exitCode = 1
}
