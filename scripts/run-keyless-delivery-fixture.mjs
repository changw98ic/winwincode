#!/usr/bin/env node

import { spawn } from 'node:child_process'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = resolve(import.meta.dirname, '..')
const scenarioPath = resolve(root, 'tests', 'fixtures', 'full-delivery-scenario.mjs')
const credentialNamePattern = /(?:API_KEY|CREDENTIAL|SECRET|TOKEN)/iu
const maximumOutputBytes = 128 * 1024 * 1024

function keylessEnvironment() {
  const environment = { ...process.env }
  for (const name of Object.keys(environment)) {
    if (credentialNamePattern.test(name)) delete environment[name]
  }
  return environment
}

function boundedAppend(current, chunk, label) {
  const next = `${current}${String(chunk)}`
  if (Buffer.byteLength(next) > maximumOutputBytes) {
    throw new Error(`keyless Delivery fixture ${label} exceeded its output limit`)
  }
  return next
}

function parseScenarioOutput(stdout) {
  const line = stdout.split('\n').find(entry => entry.trim().startsWith('{'))
  if (line === undefined) throw new Error('keyless Delivery fixture returned no JSON result')
  const result = JSON.parse(line)
  if (typeof result !== 'object'
    || result === null
    || result.finalStatus !== 'delivered'
    || result.deliveryVerdict?.status !== 'pass'
    || result.humanGate?.statusBeforeDecision !== 'needs-attention'
    || result.humanGate?.reviewStageStatus !== 'waiting'
    || result.humanGate?.executionStageCountBeforeDecision !== 0
    || !Array.isArray(result.criterionResults)
    || result.criterionResults.length === 0) {
    throw new Error('keyless Delivery fixture returned an incomplete result')
  }
  return result
}

/** Run the complete deterministic Delivery and return a concise public quickstart report. */
export function runKeylessDeliveryFixture() {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(process.execPath, [scenarioPath], {
      cwd: root,
      env: keylessEnvironment(),
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stdout = ''
    let stderr = ''
    let settled = false
    const finish = (callback) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      process.removeListener('SIGINT', forwardSigint)
      process.removeListener('SIGTERM', forwardSigterm)
      callback()
    }
    const fail = (error) => finish(() => rejectPromise(error))
    const forwardSigint = () => child.kill('SIGINT')
    const forwardSigterm = () => child.kill('SIGTERM')
    process.once('SIGINT', forwardSigint)
    process.once('SIGTERM', forwardSigterm)
    const timer = setTimeout(() => {
      child.kill('SIGKILL')
      fail(new Error('keyless Delivery fixture exceeded 90 seconds'))
    }, 90_000)

    child.stdout.on('data', (chunk) => {
      try {
        stdout = boundedAppend(stdout, chunk, 'stdout')
      } catch (error) {
        child.kill('SIGKILL')
        fail(error)
      }
    })
    child.stderr.on('data', (chunk) => {
      try {
        stderr = boundedAppend(stderr, chunk, 'stderr')
      } catch (error) {
        child.kill('SIGKILL')
        fail(error)
      }
    })
    child.once('error', fail)
    child.once('close', (code, signal) => {
      if (code !== 0 || signal !== null) {
        fail(new Error([
          `keyless Delivery fixture ended with code=${String(code)} signal=${signal ?? 'none'}`,
          stderr.trim(),
        ].filter(Boolean).join('\n')))
        return
      }
      try {
        const result = parseScenarioOutput(stdout)
        const summary = Object.freeze({
          schemaVersion: 1,
          kind: 'winwincode.keyless-delivery-fixture',
          deliveryId: result.deliveryId,
          finalStatus: result.finalStatus,
          humanGate: result.humanGate,
          planReview: result.planReview,
          candidates: result.candidates,
          criterionResults: result.criterionResults,
          deliveryVerdict: result.deliveryVerdict,
          evidenceCount: result.evidenceCount,
          projectedSubagentCount: result.projectedSubagentCount,
          modelCalls: result.modelCalls,
          measures: {
            outcome: result.measures.outcome.classification.value,
            completeness: result.measures.dimensions.completeness.status.value,
            confidence: result.measures.dimensions.confidence.status.value,
            stability: result.measures.dimensions.stability.status.value,
            totalTokens: result.measures.dimensions.efficiency.totalTokens.value,
          },
          credentialNames: result.credentialNames,
        })
        finish(() => resolvePromise(summary))
      } catch (error) {
        fail(error)
      }
    })
  })
}

export async function main() {
  const summary = await runKeylessDeliveryFixture()
  process.stdout.write(`${JSON.stringify(summary, null, 2)}\n`)
}

if (process.argv[1] !== undefined && fileURLToPath(import.meta.url) === resolve(process.argv[1])) {
  main().catch((error) => {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`)
    process.exitCode = 1
  })
}

