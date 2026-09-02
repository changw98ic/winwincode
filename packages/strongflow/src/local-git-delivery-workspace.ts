import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, realpath, stat } from 'node:fs/promises'
import { isAbsolute, join, resolve } from 'node:path'

import {
  parseDelivery,
  type Delivery,
  type FreezeDeliveryCandidateInput,
  type FrozenDeliveryCandidate,
  type SessionBinding,
  type StageRun,
} from '@winwincode/contracts'

import {
  assertFrozenDeliveryCandidateCurrent,
  freezeDeliveryCandidate,
} from './candidate-evidence.js'

const MAX_GIT_OUTPUT_BYTES = 64 * 1024 * 1024
const GIT_OBJECT_PATTERN = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/u

export type LocalGitDeliveryWorkspaceErrorCode =
  | 'INVALID_OPTIONS'
  | 'UNSUPPORTED_REPOSITORY'
  | 'BASE_REVISION_MISSING'
  | 'WORKSPACE_CONFLICT'
  | 'CANDIDATE_MISSING'
  | 'CANDIDATE_DIRTY'
  | 'CANDIDATE_DIVERGED'
  | 'GIT_FAILED'
  | 'OPERATION_ABORTED'

export class LocalGitDeliveryWorkspaceError extends Error {
  readonly code: LocalGitDeliveryWorkspaceErrorCode

  constructor(
    code: LocalGitDeliveryWorkspaceErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'LocalGitDeliveryWorkspaceError'
    this.code = code
  }
}

export interface LocalGitDeliveryWorkspaceOptions {
  readonly home: string
}

export interface PreparedLocalGitDeliveryWorkspace {
  readonly path: string
  readonly repositoryPath: string
  readonly baseCommitId: string
  readonly baseTreeId: string
  readonly candidateRefName: string
}

export interface LocalGitCandidateFacts {
  readonly workspace: PreparedLocalGitDeliveryWorkspace
  readonly baseCommitId: string
  readonly baseTreeId: string
  readonly candidateCommitId: string
  readonly candidateTreeId: string
  readonly diffSha256: string
  readonly unifiedDiff: string
  readonly changedPaths: FreezeDeliveryCandidateInput['changedPaths']
}

export interface LocalGitCandidateSnapshot {
  readonly candidate: FrozenDeliveryCandidate
  readonly unifiedDiff: string
}

interface GitResult {
  readonly code: number
  readonly signal: NodeJS.Signals | null
  readonly stdout: Buffer
  readonly stderr: Buffer
}

function workspaceError(
  code: LocalGitDeliveryWorkspaceErrorCode,
  message: string,
  cause?: unknown,
): never {
  throw new LocalGitDeliveryWorkspaceError(
    code,
    message,
    cause === undefined ? undefined : { cause },
  )
}

function immutable<Value>(value: Value): Value {
  const clone = structuredClone(value)
  const pending: object[] = []
  if (typeof clone === 'object' && clone !== null) pending.push(clone)
  while (pending.length > 0) {
    const current = pending.pop()!
    if (Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

function safeDigest(value: unknown): string {
  return createHash('sha256').update(JSON.stringify(value)).digest('hex')
}

function repositoryPath(delivery: Delivery): string {
  if (delivery.spec.repository.kind !== 'local-git') {
    return workspaceError(
      'UNSUPPORTED_REPOSITORY',
      'browser-driven execution currently requires one local Git repository',
    )
  }
  const locator = delivery.spec.repository.locator
  if (!isAbsolute(locator)) {
    return workspaceError(
      'UNSUPPORTED_REPOSITORY',
      'local Git repository locator must be an absolute path',
    )
  }
  return resolve(locator)
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await stat(path)
    return true
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === 'ENOENT') return false
    throw error
  }
}

function runProcess(
  command: string,
  arguments_: readonly string[],
  options: { readonly signal?: AbortSignal } = {},
): Promise<GitResult> {
  return new Promise((resolvePromise, reject) => {
    if (options.signal?.aborted === true) {
      reject(new LocalGitDeliveryWorkspaceError(
        'OPERATION_ABORTED',
        'local Git workspace operation was aborted',
      ))
      return
    }
    const child = spawn(command, arguments_, {
      stdio: ['ignore', 'pipe', 'pipe'],
      signal: options.signal,
      windowsHide: true,
    })
    const stdout: Buffer[] = []
    const stderr: Buffer[] = []
    let outputBytes = 0
    let outputExceeded = false
    const collect = (target: Buffer[], chunk: Buffer): void => {
      outputBytes += chunk.length
      if (outputBytes > MAX_GIT_OUTPUT_BYTES) {
        outputExceeded = true
        child.kill('SIGKILL')
        return
      }
      target.push(chunk)
    }
    child.stdout.on('data', (chunk: Buffer) => collect(stdout, chunk))
    child.stderr.on('data', (chunk: Buffer) => collect(stderr, chunk))
    child.once('error', (error) => {
      if (error.name === 'AbortError' || options.signal?.aborted === true) {
        reject(new LocalGitDeliveryWorkspaceError(
          'OPERATION_ABORTED',
          'local Git workspace operation was aborted',
          { cause: error },
        ))
        return
      }
      reject(new LocalGitDeliveryWorkspaceError(
        'GIT_FAILED',
        'local Git process could not be started',
        { cause: error },
      ))
    })
    child.once('close', (code, signal) => {
      if (outputExceeded) {
        reject(new LocalGitDeliveryWorkspaceError(
          'GIT_FAILED',
          'local Git output exceeded the bounded process limit',
        ))
        return
      }
      resolvePromise(Object.freeze({
        code: code ?? 1,
        signal,
        stdout: Buffer.concat(stdout),
        stderr: Buffer.concat(stderr),
      }))
    })
  })
}

async function gitResult(
  repository: string,
  arguments_: readonly string[],
  signal?: AbortSignal,
): Promise<GitResult> {
  return runProcess(
    'git',
    ['-C', repository, ...arguments_],
    signal === undefined ? {} : { signal },
  )
}

async function git(
  repository: string,
  arguments_: readonly string[],
  signal?: AbortSignal,
): Promise<Buffer> {
  const result = await gitResult(repository, arguments_, signal)
  if (result.code !== 0 || result.signal !== null) {
    return workspaceError(
      'GIT_FAILED',
      `git ${arguments_[0] ?? 'operation'} failed in the local Delivery repository`,
    )
  }
  return result.stdout
}

function text(output: Buffer): string {
  return output.toString('utf8').trim()
}

function assertGitObject(value: string, label: string): string {
  if (!GIT_OBJECT_PATTERN.test(value)) {
    return workspaceError('GIT_FAILED', `${label} is not a canonical Git object id`)
  }
  return value
}

function latestCandidateWriter(delivery: Delivery): {
  readonly run: StageRun
  readonly binding: SessionBinding
} | null {
  const run = delivery.stageRuns.findLast(entry => (
    entry.actorType === 'codex'
    && (entry.stage === 'executing' || entry.stage === 'reworking')
    && entry.status === 'succeeded'
  ))
  if (run === undefined) return null
  const bindings = delivery.sessionBindings.filter(entry => entry.stageRunId === run.id)
  if (bindings.length !== 1 || bindings[0]!.codexSessionId === null) {
    return workspaceError(
      'WORKSPACE_CONFLICT',
      `candidate writer StageRun ${run.id} does not have one complete SessionBinding`,
    )
  }
  return Object.freeze({ run, binding: bindings[0]! })
}

/** Isolated persistent Git worktree used by candidate-writing and read-only verification roles. */
export class LocalGitDeliveryWorkspace {
  readonly home: string

  constructor(options: LocalGitDeliveryWorkspaceOptions) {
    if (typeof options?.home !== 'string' || options.home.length === 0) {
      throw new LocalGitDeliveryWorkspaceError(
        'INVALID_OPTIONS',
        'local Git Delivery workspace requires a durable home path',
      )
    }
    this.home = resolve(options.home)
  }

  async prepare(
    deliveryValue: Delivery,
    options: { readonly signal?: AbortSignal } = {},
  ): Promise<PreparedLocalGitDeliveryWorkspace> {
    let delivery: Delivery
    try {
      delivery = parseDelivery(deliveryValue)
    } catch (error) {
      return workspaceError('INVALID_OPTIONS', 'Delivery is invalid', error)
    }
    const source = repositoryPath(delivery)
    const base = text(await git(
      source,
      ['rev-parse', '--verify', `${delivery.spec.baseRevision}^{commit}`],
      options.signal,
    ))
    if (!GIT_OBJECT_PATTERN.test(base)) {
      return workspaceError(
        'BASE_REVISION_MISSING',
        'DeliverySpec base revision does not resolve to one Git commit',
      )
    }
    const key = safeDigest({
      deliveryId: delivery.id,
      deliverySpecId: delivery.spec.id,
      deliverySpecRevision: delivery.spec.revision,
      repository: source,
      baseCommitId: base,
    })
    const root = join(this.home, 'strongflow-workspaces')
    const workspacePath = join(root, key)
    const candidateRefName = `refs/winwincode/deliveries/${key}`
    const refResult = await gitResult(
      source,
      ['show-ref', '--verify', '--quiet', candidateRefName],
      options.signal,
    )
    if (refResult.signal !== null || (refResult.code !== 0 && refResult.code !== 1)) {
      return workspaceError('GIT_FAILED', 'candidate Git reference could not be read')
    }
    const candidateCommitId = refResult.code === 0
      ? assertGitObject(
        text(await git(source, ['rev-parse', '--verify', `${candidateRefName}^{commit}`], options.signal)),
        'candidate reference',
      )
      : null
    const expectedHead = candidateCommitId ?? base
    await mkdir(root, { recursive: true, mode: 0o700 })
    if (!(await pathExists(workspacePath))) {
      await git(source, ['worktree', 'prune'], options.signal)
      await git(source, ['worktree', 'add', '--detach', workspacePath, expectedHead], options.signal)
      await git(
        source,
        ['worktree', 'lock', '--reason', `WinWinCode Delivery ${delivery.id}`, workspacePath],
        options.signal,
      )
    }
    const actualHead = text(await git(workspacePath, ['rev-parse', 'HEAD'], options.signal))
    const topLevel = await realpath(resolve(text(await git(
      workspacePath,
      ['rev-parse', '--show-toplevel'],
      options.signal,
    ))))
    const physicalWorkspacePath = await realpath(workspacePath)
    const baseIsAncestor = await gitResult(
      workspacePath,
      ['merge-base', '--is-ancestor', base, actualHead],
      options.signal,
    )
    if (topLevel !== physicalWorkspacePath
      || actualHead !== expectedHead
      || baseIsAncestor.code !== 0
      || baseIsAncestor.signal !== null) {
      return workspaceError(
        'WORKSPACE_CONFLICT',
        'existing Delivery worktree does not match its pinned base or candidate commit',
      )
    }
    return immutable({
      path: workspacePath,
      repositoryPath: source,
      baseCommitId: base,
      baseTreeId: assertGitObject(
        text(await git(workspacePath, ['rev-parse', `${base}^{tree}`], options.signal)),
        'base tree',
      ),
      candidateRefName,
    })
  }

  async freezeCandidateFacts(
    delivery: Delivery,
    options: { readonly signal?: AbortSignal; readonly commitMessage?: string } = {},
  ): Promise<LocalGitCandidateFacts> {
    const workspace = await this.prepare(delivery, options)
    const status = await git(
      workspace.path,
      ['status', '--porcelain=v1', '-z', '--untracked-files=all'],
      options.signal,
    )
    if (status.length > 0) {
      await git(workspace.path, ['add', '--all'], options.signal)
      const staged = await git(
        workspace.path,
        ['diff', '--cached', '--name-only', '-z'],
        options.signal,
      )
      if (staged.length === 0) {
        return workspaceError('CANDIDATE_MISSING', 'candidate changes produced no staged path')
      }
      const commitMessage = options.commitMessage
        ?? `WinWinCode Delivery ${delivery.id} revision ${String(delivery.revision)}`
      if (commitMessage.trim().length === 0 || commitMessage.length > 500) {
        return workspaceError('INVALID_OPTIONS', 'candidate commit message is invalid')
      }
      await git(workspace.path, [
        '-c', 'user.name=WinWinCode',
        '-c', 'user.email=delivery@winwincode.invalid',
        '-c', 'commit.gpgSign=false',
        '-c', 'core.hooksPath=/dev/null',
        'commit',
        '--no-verify',
        '-m', commitMessage,
      ], options.signal)
      const candidateCommitId = assertGitObject(
        text(await git(workspace.path, ['rev-parse', 'HEAD'], options.signal)),
        'candidate commit',
      )
      await git(
        workspace.repositoryPath,
        ['update-ref', workspace.candidateRefName, candidateCommitId],
        options.signal,
      )
    }
    const facts = await this.readCandidateFacts(delivery, options)
    if (facts === null) {
      return workspaceError('CANDIDATE_MISSING', 'executor produced no Git candidate change')
    }
    return facts
  }

  async readCandidateFacts(
    delivery: Delivery,
    options: { readonly signal?: AbortSignal } = {},
  ): Promise<LocalGitCandidateFacts | null> {
    const workspace = await this.prepare(delivery, options)
    const refResult = await gitResult(
      workspace.repositoryPath,
      ['show-ref', '--verify', '--quiet', workspace.candidateRefName],
      options.signal,
    )
    if (refResult.code === 1 && refResult.signal === null) return null
    if (refResult.code !== 0 || refResult.signal !== null) {
      return workspaceError('GIT_FAILED', 'candidate Git reference could not be read')
    }
    const candidateCommitId = assertGitObject(
      text(await git(
        workspace.repositoryPath,
        ['rev-parse', '--verify', `${workspace.candidateRefName}^{commit}`],
        options.signal,
      )),
      'candidate commit',
    )
    const head = text(await git(workspace.path, ['rev-parse', 'HEAD'], options.signal))
    if (head !== candidateCommitId) {
      return workspaceError('WORKSPACE_CONFLICT', 'Delivery worktree is not on its candidate ref')
    }
    const status = await git(
      workspace.path,
      ['status', '--porcelain=v1', '-z', '--untracked-files=all'],
      options.signal,
    )
    if (status.length > 0) {
      return workspaceError('CANDIDATE_DIRTY', 'frozen candidate worktree contains later changes')
    }
    const ancestor = await gitResult(
      workspace.path,
      ['merge-base', '--is-ancestor', workspace.baseCommitId, candidateCommitId],
      options.signal,
    )
    if (ancestor.code !== 0 || ancestor.signal !== null) {
      return workspaceError('CANDIDATE_DIVERGED', 'candidate is not based on the pinned commit')
    }
    const diff = await git(workspace.path, [
      'diff',
      '--no-ext-diff',
      '--binary',
      '--full-index',
      `${workspace.baseCommitId}..${candidateCommitId}`,
    ], options.signal)
    const pathOutput = await git(
      workspace.path,
      ['diff', '--name-only', '-z', `${workspace.baseCommitId}..${candidateCommitId}`],
      options.signal,
    )
    const paths = pathOutput.toString('utf8').split('\u0000').filter(Boolean)
    if (paths.length === 0) {
      return workspaceError('CANDIDATE_MISSING', 'candidate commit changes no path')
    }
    const changedPaths: FreezeDeliveryCandidateInput['changedPaths'][number][] = []
    for (const path of paths) {
      const object = await gitResult(
        workspace.path,
        ['rev-parse', '--verify', `${candidateCommitId}:${path}`],
        options.signal,
      )
      if (object.signal !== null || (object.code !== 0 && object.code !== 128)) {
        return workspaceError('GIT_FAILED', `candidate path ${path} could not be resolved`)
      }
      changedPaths.push(Object.freeze({
        path,
        state: object.code === 0 ? 'present' : 'deleted',
        objectId: object.code === 0
          ? assertGitObject(text(object.stdout), `candidate path ${path}`)
          : null,
      }))
    }
    changedPaths.sort((left, right) => left.path.localeCompare(right.path))
    return immutable({
      workspace,
      baseCommitId: workspace.baseCommitId,
      baseTreeId: workspace.baseTreeId,
      candidateCommitId,
      candidateTreeId: assertGitObject(
        text(await git(workspace.path, ['rev-parse', `${candidateCommitId}^{tree}`], options.signal)),
        'candidate tree',
      ),
      diffSha256: createHash('sha256').update(diff).digest('hex'),
      unifiedDiff: diff.toString('utf8'),
      changedPaths,
    })
  }

  async currentCandidateSnapshot(
    deliveryValue: Delivery,
    options: { readonly signal?: AbortSignal } = {},
  ): Promise<LocalGitCandidateSnapshot | null> {
    const delivery = parseDelivery(deliveryValue)
    if (delivery.stageRuns.some(run => (
      run.status === 'running'
      && run.actorType === 'codex'
      && (run.stage === 'executing' || run.stage === 'reworking')
    ))) return null
    const writer = latestCandidateWriter(delivery)
    if (writer === null) return null
    const facts = await this.readCandidateFacts(delivery, options)
    if (facts === null) return null
    return immutable({
      candidate: freezeDeliveryCandidate(delivery, {
        producerStageRunId: writer.run.id,
        producerSessionBindingId: writer.binding.id,
        baseCommitId: facts.baseCommitId,
        baseTreeId: facts.baseTreeId,
        candidateCommitId: facts.candidateCommitId,
        candidateTreeId: facts.candidateTreeId,
        diffSha256: facts.diffSha256,
        changedPaths: facts.changedPaths,
      }),
      unifiedDiff: facts.unifiedDiff,
    })
  }

  async currentCandidate(
    deliveryValue: Delivery,
    options: { readonly signal?: AbortSignal } = {},
  ): Promise<FrozenDeliveryCandidate | null> {
    return (await this.currentCandidateSnapshot(deliveryValue, options))?.candidate ?? null
  }

  async assertCandidate(
    delivery: Delivery,
    candidate: FrozenDeliveryCandidate,
    options: { readonly signal?: AbortSignal } = {},
  ): Promise<void> {
    const current = await this.currentCandidate(delivery, options)
    if (current === null
      || current.candidateRef !== assertFrozenDeliveryCandidateCurrent(
        delivery,
        candidate,
      ).candidateRef) {
      return workspaceError('WORKSPACE_CONFLICT', 'candidate no longer matches its Git worktree')
    }
  }
}
