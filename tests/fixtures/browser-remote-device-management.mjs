// SPDX-License-Identifier: Apache-2.0

import { mountWinWinCodeClient } from '/module/application.js'
import { mountCandidateApplication } from '/module/candidate-application.js'

const schemaVersion = 'winwincode/v1'
const clientNodeId = 'cnd_00000000000000000000000001'
const baseCommit = 'a'.repeat(40)
const candidateCommit = 'b'.repeat(40)
const candidateRef = `refs/winwincode/candidates/${candidateCommit}`
const scope = { kind: 'repository', organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001', projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001' }
const calls = { pairing: 0, revocations: [], applications: [] }
const device = { clientId: '123456789012', clientNodeId, displayName: 'Remote Linux · fixture',
  presence: 'online', occupancy: 'available', capacityUsed: 0, capacityTotal: 1,
  lastHeartbeatAt: new Date().toISOString(), version: '0.2.0' }
const repository = { repositoryBindingId: 'rbd_00000000000000000000000001', displayName: 'Remote project',
  defaultBranch: 'main', headCommit: baseCommit, dirtyState: 'clean', availability: 'available' }

const client = {
  serverUrl: 'https://server.fixture',
  async restore() { return { schemaVersion, expiresAt: '2099-10-15T00:00:00.000Z',
    actor: { kind: 'user', id: 'usr_00000000000000000000000001' }, authorizedScopes: [scope] } },
  async initializationStatus() { return { initialized: true } },
  async listClients() { return [device] },
  async listRepositories() { return [repository] },
  async createDevicePairing() {
    calls.pairing += 1
    return { schemaVersion, pairingToken: 'synthetic-pairing-token-fixture', expiresAt: '2099-10-15T00:00:00.000Z' }
  },
  async revokeDevice(node) { calls.revocations.push(node); return { schemaVersion, clientNodeId: node, revoked: true } },
  async query(request) { return { schemaVersion, requestId: request.requestId, query: request.query,
    result: { kind: request.query === 'worker.list' ? 'worker_page' : 'product_session_page', items: [] },
    page: { hasMore: false, nextCursor: null } } },
  subscribe() { return { close() {} } },
  close() {},
}
location.hash = '#/device'
const application = mountWinWinCodeClient({ root: document.querySelector('[data-winwincode-client-root]'),
  serverUrl: client.serverUrl, controlPlane: client })

async function until(check) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    if (check()) return
    await new Promise(resolve => setTimeout(resolve, 10))
  }
  throw new Error('Remote Device fixture did not settle')
}

await until(() => document.querySelector('.wwc-device .wwc-clients-card-revoke'))
const management = () => document.querySelector('.wwc-device .wwc-clients')
globalThis.remoteDeviceReady = () => true
globalThis.pairRemoteDeviceFixture = async () => {
  management().querySelector('.wwc-clients-pair').click()
  await until(() => management().querySelector('textarea').value.length > 0)
  return { hash: location.hash, visible: !management().hidden, calls: calls.pairing,
    command: management().querySelector('textarea').value,
    expiry: management().querySelector('.wwc-clients-pairing-expiry').textContent }
}
globalThis.revokeRemoteDeviceFixture = async () => {
  management().querySelector('.wwc-clients-card-revoke').click()
  const beforeConfirmation = calls.revocations.length
  const confirmationVisible = !management().querySelector('.wwc-clients-revoke-confirmation').hidden
  management().querySelector('.wwc-clients-confirm-revoke').click()
  await until(() => management().querySelector('.wwc-clients-management-status').textContent.includes('已永久吊销'))
  return { beforeConfirmation, confirmationVisible, calls: [...calls.revocations],
    status: management().querySelector('.wwc-clients-management-status').textContent }
}

let candidateView
globalThis.showCandidateApplicationFixture = async status => {
  candidateView?.close()
  let currentStatus = status
  let root = document.querySelector('#candidate-application-fixture')
  if (root === null) {
    root = document.createElement('section')
    root.id = 'candidate-application-fixture'
    document.body.append(root)
  }
  const candidate = { candidateId: 'cnd_00000000000000000000000002', candidateRef,
    candidateCommitId: candidateCommit, deliverySpecId: 'spec-fixture', deliverySpecRevision: 1 }
  const state = { status: 'ready', detail: { currentCandidate: candidate,
    requirements: { deliverySpecId: 'spec-fixture', deliverySpecRevision: 1, acceptanceCriteria: [] }, evidence: [],
    verdict: { candidateId: candidate.candidateId, candidateRef, snapshotId: 'snapshot-fixture',
      deliverySpecId: 'spec-fixture', deliverySpecRevision: 1, status: 'pass', unresolvedFindings: [] } } }
  const local = () => ({ localCandidateReceiptId: 'receipt-fixture', candidateRef,
    repositoryBindingId: repository.repositoryBindingId, candidateCommit, baseCommit,
    localRefName: candidateRef, state: 'retained', createdAt: '2026-10-15T00:00:00.000Z', revision: 1,
    branchName: null, history: [], application: { applicationId: 'lar_00000000000000000000000001',
      approvalId: 'approval-fixture', baseCommit, candidateDigest: `sha256:${'c'.repeat(64)}`,
      targetBranch: 'release/original', status: currentStatus } })
  candidateView = mountCandidateApplication({ root,
    model: { state, subscribe(listener) { listener(state); return () => {} }, async refresh() {}, async acceptDelivery() {} },
    directory: client, async claim() {},
    candidates: { async listDeviceCandidates() { return [local()] },
      async applyCandidate(input) {
        calls.applications.push(input); currentStatus = 'applied'
        return { ...input, result: 'applied', resultingCommit: candidateCommit, conflictArtifactRef: null }
      } },
  })
  await until(() => root.querySelector('select').options.length === 1)
  root.scrollIntoView()
  return { status: root.querySelector('[role=status]').textContent,
    disabled: root.querySelector('button[type=submit]').disabled,
    branch: root.querySelector('#wwc-apply-branch').value,
    head: root.querySelector('#wwc-apply-head').value }
}
globalThis.submitCandidateApplicationFixture = async () => {
  const root = document.querySelector('#candidate-application-fixture')
  root.querySelector('input[type=checkbox]').checked = true
  root.querySelector('form').dispatchEvent(new Event('submit', { cancelable: true }))
  await until(() => root.querySelector('[role=status]').textContent.includes('已确认应用完成'))
  return { applications: [...calls.applications], disabled: root.querySelector('button[type=submit]').disabled }
}
globalThis.closeRemoteDeviceFixture = () => { candidateView?.close(); application.close() }
