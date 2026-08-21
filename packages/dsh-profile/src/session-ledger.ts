import { createHash, randomUUID } from 'node:crypto'
import {
  mkdir,
  open,
  readFile,
  rename,
  rm,
  stat,
} from 'node:fs/promises'
import { dirname, join, resolve } from 'node:path'

import {
  RUNTIME_EVENT_SCHEMA_VERSION,
  type RuntimeEvent,
} from '@winwincode/contracts'

export const RUNTIME_SESSION_LEDGER_SCHEMA_VERSION = 1 as const

export type RuntimeSessionLedgerErrorCode =
  | 'INVALID_LEDGER_OPTIONS'
  | 'LEDGER_ALREADY_EXISTS'
  | 'LEDGER_NOT_FOUND'
  | 'LEDGER_CORRUPT'
  | 'LEDGER_SESSION_MISMATCH'
  | 'LEDGER_SEQUENCE_MISMATCH'

/** Visible failure at the durable DSH-to-kernel session mapping boundary. */
export class RuntimeSessionLedgerError extends Error {
  readonly code: RuntimeSessionLedgerErrorCode

  constructor(code: RuntimeSessionLedgerErrorCode, message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'RuntimeSessionLedgerError'
    this.code = code
  }
}

export interface RuntimeSessionManifest {
  readonly schemaVersion: typeof RUNTIME_SESSION_LEDGER_SCHEMA_VERSION
  readonly dshSessionId: string
  readonly roleId: string
  readonly cwd: string
  readonly kernelSessionId: string
  readonly kernelStreamId: string
  readonly rolloutPath: string
  readonly provider: string
  readonly model: string
}

export interface RuntimeKernelLifecycle {
  readonly kernelSessionId: string
  readonly kernelStreamId: string
  readonly rolloutPath: string
  readonly provider: string
  readonly model: string
}

export interface RuntimeKernelLifecycleRecord extends RuntimeKernelLifecycle {
  readonly schemaVersion: typeof RUNTIME_SESSION_LEDGER_SCHEMA_VERSION
  readonly recordType: 'kernel.lifecycle'
  readonly dshSessionId: string
  readonly roleId: string
  readonly cwd: string
}

export interface RuntimeEventLedgerRecord {
  readonly schemaVersion: typeof RUNTIME_SESSION_LEDGER_SCHEMA_VERSION
  readonly recordType: 'runtime.event'
  readonly event: RuntimeEvent
}

export type RuntimeSessionLedgerRecord =
  | RuntimeKernelLifecycleRecord
  | RuntimeEventLedgerRecord

export interface CreateRuntimeSessionLedgerOptions extends RuntimeKernelLifecycle {
  readonly home: string
  readonly dshSessionId: string
  readonly roleId: string
  readonly cwd: string
}

export interface RuntimeSessionLedgerSnapshot {
  readonly manifest: RuntimeSessionManifest
  readonly records: readonly RuntimeSessionLedgerRecord[]
  readonly events: readonly RuntimeEvent[]
}

function nonEmpty(value: string, label: string): string {
  if (value.length === 0) throw new Error(`${label} must not be empty`)
  return value
}

function sessionKey(sessionId: string): string {
  return createHash('sha256').update(sessionId).digest('hex')
}

function ledgerDirectory(home: string, sessionId: string): string {
  return join(resolve(home), 'runtime-sessions', sessionKey(sessionId))
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function stringField(record: Record<string, unknown>, key: string): string {
  const value = record[key]
  if (typeof value !== 'string' || value.length === 0) {
    throw new Error(`${key} must be a non-empty string`)
  }
  return value
}

function parseManifest(value: unknown): RuntimeSessionManifest {
  if (!isRecord(value) || value.schemaVersion !== RUNTIME_SESSION_LEDGER_SCHEMA_VERSION) {
    throw new Error('unsupported or missing manifest schemaVersion')
  }
  return Object.freeze({
    schemaVersion: RUNTIME_SESSION_LEDGER_SCHEMA_VERSION,
    dshSessionId: stringField(value, 'dshSessionId'),
    roleId: stringField(value, 'roleId'),
    cwd: stringField(value, 'cwd'),
    kernelSessionId: stringField(value, 'kernelSessionId'),
    kernelStreamId: stringField(value, 'kernelStreamId'),
    rolloutPath: stringField(value, 'rolloutPath'),
    provider: stringField(value, 'provider'),
    model: stringField(value, 'model'),
  })
}

function parseRuntimeEvent(value: unknown): RuntimeEvent {
  if (!isRecord(value)
    || value.schemaVersion !== RUNTIME_EVENT_SCHEMA_VERSION
    || typeof value.id !== 'string'
    || !isRecord(value.cursor)
    || typeof value.cursor.sessionId !== 'string'
    || typeof value.cursor.sequence !== 'string'
    || !/^\d+$/u.test(value.cursor.sequence)
    || typeof value.kind !== 'string'
    || !isRecord(value.source)
    || value.source.authority !== 'codex-core'
    || typeof value.source.sessionId !== 'string'
    || typeof value.source.kernelSessionId !== 'string'
    || typeof value.source.roleId !== 'string'
    || !isRecord(value.data)) {
    throw new Error('invalid runtime event')
  }
  return structuredClone(value) as unknown as RuntimeEvent
}

function parseRecord(value: unknown): RuntimeSessionLedgerRecord {
  if (!isRecord(value) || value.schemaVersion !== RUNTIME_SESSION_LEDGER_SCHEMA_VERSION) {
    throw new Error('unsupported or missing record schemaVersion')
  }
  if (value.recordType === 'kernel.lifecycle') {
    return Object.freeze({
      schemaVersion: RUNTIME_SESSION_LEDGER_SCHEMA_VERSION,
      recordType: 'kernel.lifecycle',
      dshSessionId: stringField(value, 'dshSessionId'),
      roleId: stringField(value, 'roleId'),
      cwd: stringField(value, 'cwd'),
      kernelSessionId: stringField(value, 'kernelSessionId'),
      kernelStreamId: stringField(value, 'kernelStreamId'),
      rolloutPath: stringField(value, 'rolloutPath'),
      provider: stringField(value, 'provider'),
      model: stringField(value, 'model'),
    })
  }
  if (value.recordType === 'runtime.event') {
    return Object.freeze({
      schemaVersion: RUNTIME_SESSION_LEDGER_SCHEMA_VERSION,
      recordType: 'runtime.event',
      event: parseRuntimeEvent(value.event),
    })
  }
  throw new Error('unknown ledger recordType')
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await stat(path)
    return true
  } catch (error) {
    if (isRecord(error) && error.code === 'ENOENT') return false
    throw error
  }
}

async function syncDirectory(path: string): Promise<void> {
  const directory = await open(path, 'r')
  try {
    await directory.sync()
  } finally {
    await directory.close()
  }
}

async function writeAtomic(path: string, value: unknown): Promise<void> {
  const temporary = `${path}.${randomUUID()}.tmp`
  const handle = await open(temporary, 'wx', 0o600)
  try {
    await handle.writeFile(`${JSON.stringify(value, null, 2)}\n`, 'utf8')
    await handle.sync()
  } finally {
    await handle.close()
  }
  try {
    await rename(temporary, path)
    await syncDirectory(dirname(path))
  } catch (error) {
    await rm(temporary, { force: true })
    throw error
  }
}

async function appendDurable(path: string, value: unknown): Promise<void> {
  const handle = await open(path, 'a', 0o600)
  try {
    await handle.writeFile(`${JSON.stringify(value)}\n`, 'utf8')
    await handle.sync()
  } finally {
    await handle.close()
  }
}

function manifestFrom(
  base: Pick<RuntimeSessionManifest, 'dshSessionId' | 'roleId' | 'cwd'>,
  lifecycle: RuntimeKernelLifecycle,
): RuntimeSessionManifest {
  return Object.freeze({
    schemaVersion: RUNTIME_SESSION_LEDGER_SCHEMA_VERSION,
    dshSessionId: base.dshSessionId,
    roleId: base.roleId,
    cwd: base.cwd,
    kernelSessionId: lifecycle.kernelSessionId,
    kernelStreamId: lifecycle.kernelStreamId,
    rolloutPath: lifecycle.rolloutPath,
    provider: lifecycle.provider,
    model: lifecycle.model,
  })
}

/** Append-only runtime history stored separately from the stock DSH session log. */
export class RuntimeSessionLedger {
  readonly directory: string
  readonly manifestPath: string
  readonly recordsPath: string
  #manifest: RuntimeSessionManifest
  #tail: Promise<void> = Promise.resolve()

  private constructor(directory: string, manifest: RuntimeSessionManifest) {
    this.directory = directory
    this.manifestPath = join(directory, 'manifest.json')
    this.recordsPath = join(directory, 'runtime.jsonl')
    this.#manifest = manifest
  }

  static async create(options: CreateRuntimeSessionLedgerOptions): Promise<RuntimeSessionLedger> {
    let validated: CreateRuntimeSessionLedgerOptions
    try {
      validated = {
        home: nonEmpty(options.home, 'home'),
        dshSessionId: nonEmpty(options.dshSessionId, 'dshSessionId'),
        roleId: nonEmpty(options.roleId, 'roleId'),
        cwd: nonEmpty(options.cwd, 'cwd'),
        kernelSessionId: nonEmpty(options.kernelSessionId, 'kernelSessionId'),
        kernelStreamId: nonEmpty(options.kernelStreamId, 'kernelStreamId'),
        rolloutPath: nonEmpty(options.rolloutPath, 'rolloutPath'),
        provider: nonEmpty(options.provider, 'provider'),
        model: nonEmpty(options.model, 'model'),
      }
    } catch (error) {
      throw new RuntimeSessionLedgerError(
        'INVALID_LEDGER_OPTIONS',
        error instanceof Error ? error.message : 'invalid ledger options',
      )
    }
    const directory = ledgerDirectory(validated.home, validated.dshSessionId)
    const manifestPath = join(directory, 'manifest.json')
    await mkdir(dirname(directory), { recursive: true, mode: 0o700 })
    if (await pathExists(manifestPath)) {
      throw new RuntimeSessionLedgerError(
        'LEDGER_ALREADY_EXISTS',
        `runtime ledger already exists for DSH session ${validated.dshSessionId}`,
      )
    }
    await mkdir(directory, { recursive: true, mode: 0o700 })
    const manifest = manifestFrom(validated, validated)
    const ledger = new RuntimeSessionLedger(directory, manifest)
    try {
      await writeAtomic(manifestPath, manifest)
      await appendDurable(ledger.recordsPath, ledger.#lifecycleRecord(validated))
      await syncDirectory(directory)
      return ledger
    } catch (error) {
      await rm(directory, { recursive: true, force: true })
      throw error
    }
  }

  static async open(home: string, dshSessionId: string): Promise<RuntimeSessionLedger> {
    try {
      nonEmpty(home, 'home')
      nonEmpty(dshSessionId, 'dshSessionId')
    } catch (error) {
      throw new RuntimeSessionLedgerError(
        'INVALID_LEDGER_OPTIONS',
        error instanceof Error ? error.message : 'invalid ledger options',
      )
    }
    const directory = ledgerDirectory(home, dshSessionId)
    const manifestPath = join(directory, 'manifest.json')
    if (!(await pathExists(manifestPath))) {
      throw new RuntimeSessionLedgerError(
        'LEDGER_NOT_FOUND',
        `runtime ledger was not found for DSH session ${dshSessionId}`,
      )
    }
    try {
      const manifest = parseManifest(JSON.parse(await readFile(manifestPath, 'utf8')) as unknown)
      if (manifest.dshSessionId !== dshSessionId) {
        throw new RuntimeSessionLedgerError(
          'LEDGER_SESSION_MISMATCH',
          `runtime ledger belongs to DSH session ${manifest.dshSessionId}, not ${dshSessionId}`,
        )
      }
      const ledger = new RuntimeSessionLedger(directory, manifest)
      const snapshot = await ledger.read()
      const latestLifecycle = snapshot.records.findLast(
        (record): record is RuntimeKernelLifecycleRecord => record.recordType === 'kernel.lifecycle',
      )
      if (latestLifecycle === undefined) throw new Error('ledger has no kernel lifecycle record')
      const recovered = manifestFrom(manifest, latestLifecycle)
      if (JSON.stringify(recovered) !== JSON.stringify(manifest)) {
        await writeAtomic(manifestPath, recovered)
        ledger.#manifest = recovered
      }
      return ledger
    } catch (error) {
      if (error instanceof RuntimeSessionLedgerError) throw error
      throw new RuntimeSessionLedgerError(
        'LEDGER_CORRUPT',
        `runtime ledger for DSH session ${dshSessionId} is corrupt`,
        { cause: error },
      )
    }
  }

  get manifest(): RuntimeSessionManifest {
    return structuredClone(this.#manifest)
  }

  async appendLifecycle(lifecycle: RuntimeKernelLifecycle): Promise<void> {
    await this.#serialize(async () => {
      for (const [key, value] of Object.entries(lifecycle)) nonEmpty(value, key)
      const next = manifestFrom(this.#manifest, lifecycle)
      await appendDurable(this.recordsPath, this.#lifecycleRecord(lifecycle))
      await writeAtomic(this.manifestPath, next)
      this.#manifest = next
    })
  }

  async appendEvent(event: RuntimeEvent): Promise<void> {
    await this.#serialize(async () => {
      if (event.cursor.sessionId !== this.#manifest.dshSessionId
        || event.source.sessionId !== this.#manifest.dshSessionId) {
        throw new RuntimeSessionLedgerError(
          'LEDGER_SESSION_MISMATCH',
          `runtime event ${event.id} does not belong to DSH session ${this.#manifest.dshSessionId}`,
        )
      }
      if (event.source.kernelSessionId !== this.#manifest.kernelSessionId) {
        throw new RuntimeSessionLedgerError(
          'LEDGER_SESSION_MISMATCH',
          `runtime event ${event.id} does not belong to active kernel session ${this.#manifest.kernelSessionId}`,
        )
      }
      const snapshot = await this.#readUnlocked()
      const expected = BigInt(snapshot.events.at(-1)?.cursor.sequence ?? '0') + 1n
      if (BigInt(event.cursor.sequence) !== expected) {
        throw new RuntimeSessionLedgerError(
          'LEDGER_SEQUENCE_MISMATCH',
          `runtime event ${event.id} has sequence ${event.cursor.sequence}; expected ${expected.toString()}`,
        )
      }
      await appendDurable(this.recordsPath, Object.freeze({
        schemaVersion: RUNTIME_SESSION_LEDGER_SCHEMA_VERSION,
        recordType: 'runtime.event' as const,
        event,
      }))
    })
  }

  async read(): Promise<RuntimeSessionLedgerSnapshot> {
    await this.#tail
    return this.#readUnlocked()
  }

  /** Remove an unpublished fresh-session ledger after its creation transaction rolls back. */
  async discard(): Promise<void> {
    await this.#tail
    await rm(this.directory, { recursive: true, force: true })
  }

  #lifecycleRecord(lifecycle: RuntimeKernelLifecycle): RuntimeKernelLifecycleRecord {
    return Object.freeze({
      schemaVersion: RUNTIME_SESSION_LEDGER_SCHEMA_VERSION,
      recordType: 'kernel.lifecycle',
      dshSessionId: this.#manifest.dshSessionId,
      roleId: this.#manifest.roleId,
      cwd: this.#manifest.cwd,
      ...lifecycle,
    })
  }

  async #readUnlocked(): Promise<RuntimeSessionLedgerSnapshot> {
    let text: string
    try {
      text = await readFile(this.recordsPath, 'utf8')
    } catch (error) {
      throw new RuntimeSessionLedgerError(
        'LEDGER_CORRUPT',
        `runtime ledger records are missing for DSH session ${this.#manifest.dshSessionId}`,
        { cause: error },
      )
    }
    const records: RuntimeSessionLedgerRecord[] = []
    const events: RuntimeEvent[] = []
    try {
      for (const [index, line] of text.split('\n').entries()) {
        if (line.length === 0) continue
        const record = parseRecord(JSON.parse(line) as unknown)
        if (record.recordType === 'kernel.lifecycle') {
          if (record.dshSessionId !== this.#manifest.dshSessionId
            || record.roleId !== this.#manifest.roleId
            || record.cwd !== this.#manifest.cwd) {
            throw new Error(`lifecycle identity mismatch at line ${index + 1}`)
          }
        } else {
          const expected = BigInt(events.at(-1)?.cursor.sequence ?? '0') + 1n
          if (record.event.cursor.sessionId !== this.#manifest.dshSessionId
            || record.event.source.sessionId !== this.#manifest.dshSessionId
            || BigInt(record.event.cursor.sequence) !== expected) {
            throw new Error(`runtime event identity or sequence mismatch at line ${index + 1}`)
          }
          events.push(record.event)
        }
        records.push(record)
      }
    } catch (error) {
      throw new RuntimeSessionLedgerError(
        'LEDGER_CORRUPT',
        `runtime ledger records are corrupt for DSH session ${this.#manifest.dshSessionId}`,
        { cause: error },
      )
    }
    return Object.freeze({
      manifest: this.manifest,
      records: Object.freeze(records),
      events: Object.freeze(events),
    })
  }

  #serialize(operation: () => Promise<void>): Promise<void> {
    const current = this.#tail.then(operation, operation)
    this.#tail = current.catch(() => {})
    return current
  }
}
