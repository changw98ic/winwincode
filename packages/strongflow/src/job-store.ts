import { createHash, randomUUID } from 'node:crypto'
import {
  link,
  lstat,
  mkdir,
  open,
  readFile,
  readdir,
  rename,
  rm,
} from 'node:fs/promises'
import { basename, join, resolve } from 'node:path'

import {
  JobId as parseJobId,
  applyStrongFlowJobEvent,
  assertStrongFlowJobEvent,
  projectStrongFlowJob,
  type JobDefinitionSnapshot,
  type JobId,
  type StrongFlowJobEvent,
  type StrongFlowJobSnapshot,
} from '@winwincode/contracts'

export const STRONGFLOW_JOB_STORE_SCHEMA_VERSION = 1 as const

const JOB_DIRECTORY_PATTERN = /^[a-f0-9]{64}$/u
const EVENT_FILE_PATTERN = /^([1-9][0-9]*)\.json$/u
const CREATING_DIRECTORY_PATTERN = /^\.creating-[a-f0-9]{64}-[0-9a-f-]+$/u
const PENDING_EVENT_PATTERN = /^\.pending-[1-9][0-9]*-[0-9a-f-]+\.json$/u

export type StrongFlowJobStoreErrorCode =
  | 'INVALID_STORE_OPTIONS'
  | 'JOB_ALREADY_EXISTS'
  | 'JOB_NOT_FOUND'
  | 'STORE_CORRUPT'
  | 'JOB_ID_MISMATCH'
  | 'EVENT_SEQUENCE_MISMATCH'
  | 'EVENT_ALREADY_EXISTS'
  | 'STORE_IO_ERROR'

/** Stable failure at the durable StrongFlow job boundary. */
export class StrongFlowJobStoreError extends Error {
  readonly code: StrongFlowJobStoreErrorCode

  constructor(code: StrongFlowJobStoreErrorCode, message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'StrongFlowJobStoreError'
    this.code = code
  }
}

export interface StrongFlowJobManifest {
  readonly schemaVersion: typeof STRONGFLOW_JOB_STORE_SCHEMA_VERSION
  readonly jobId: JobId
  readonly createdEventId: string
  readonly createdAtMillis: number
}

export interface CreateStrongFlowJobStoreOptions {
  readonly home: string
  readonly event: StrongFlowJobEvent
}

export interface StrongFlowStoredJob {
  readonly manifest: StrongFlowJobManifest
  readonly events: readonly StrongFlowJobEvent[]
  readonly snapshot: StrongFlowJobSnapshot
}

export interface StrongFlowJobListEntry {
  readonly manifest: StrongFlowJobManifest
  readonly sequence: string
  readonly state: StrongFlowJobSnapshot['state']
  readonly definitionRevision: number
  readonly definition: JobDefinitionSnapshot
  readonly approved: boolean
  readonly candidateId?: StrongFlowJobSnapshot['candidateId']
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function errorCode(error: unknown): string | undefined {
  if (!isRecord(error)) return undefined
  return typeof error.code === 'string' ? error.code : undefined
}

function exactKeys(
  value: Record<string, unknown>,
  expected: readonly string[],
  label: string,
): void {
  const expectedKeys = new Set(expected)
  if (
    Object.keys(value).length !== expectedKeys.size
    || expected.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expectedKeys.has(key))
  ) throw new Error(`${label} has an unexpected shape`)
}

function validateHome(home: string): string {
  if (typeof home !== 'string' || home.length === 0) {
    throw new StrongFlowJobStoreError(
      'INVALID_STORE_OPTIONS',
      'StrongFlow job home must be a non-empty path',
    )
  }
  return resolve(home)
}

function validateJobId(value: string): JobId {
  try {
    return parseJobId(value)
  } catch (error) {
    throw new StrongFlowJobStoreError(
      'INVALID_STORE_OPTIONS',
      'StrongFlow job id is invalid',
      { cause: error },
    )
  }
}

function jobsRoot(home: string): string {
  return join(validateHome(home), 'strongflow-jobs')
}

function jobKey(jobId: string): string {
  return createHash('sha256').update(jobId).digest('hex')
}

function jobDirectory(home: string, jobId: string): string {
  return join(jobsRoot(home), jobKey(jobId))
}

function immutableJson<Value>(value: Value): Value {
  if (Array.isArray(value)) {
    for (const entry of value) immutableJson(entry)
    return Object.freeze(value)
  }
  if (isRecord(value)) {
    for (const entry of Object.values(value)) immutableJson(entry)
    return Object.freeze(value) as Value
  }
  return value
}

function manifestFrom(event: StrongFlowJobEvent): StrongFlowJobManifest {
  return Object.freeze({
    schemaVersion: STRONGFLOW_JOB_STORE_SCHEMA_VERSION,
    jobId: event.jobId,
    createdEventId: event.id,
    createdAtMillis: event.occurredAtMillis,
  })
}

function parseManifest(value: unknown): StrongFlowJobManifest {
  if (!isRecord(value)) throw new Error('manifest must be an object')
  exactKeys(
    value,
    ['schemaVersion', 'jobId', 'createdEventId', 'createdAtMillis'],
    'manifest',
  )
  if (value.schemaVersion !== STRONGFLOW_JOB_STORE_SCHEMA_VERSION) {
    throw new Error('manifest schemaVersion is unsupported')
  }
  if (typeof value.jobId !== 'string') throw new Error('manifest jobId is invalid')
  const jobId = parseJobId(value.jobId)
  if (typeof value.createdEventId !== 'string' || value.createdEventId.length === 0) {
    throw new Error('manifest createdEventId is invalid')
  }
  if (
    typeof value.createdAtMillis !== 'number'
    || !Number.isSafeInteger(value.createdAtMillis)
    || value.createdAtMillis < 0
  ) throw new Error('manifest createdAtMillis is invalid')
  return Object.freeze({
    schemaVersion: STRONGFLOW_JOB_STORE_SCHEMA_VERSION,
    jobId,
    createdEventId: value.createdEventId,
    createdAtMillis: value.createdAtMillis,
  })
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await lstat(path)
    return true
  } catch (error) {
    if (errorCode(error) === 'ENOENT') return false
    throw error
  }
}

async function syncDirectory(path: string): Promise<void> {
  const handle = await open(path, 'r')
  try {
    await handle.sync()
  } finally {
    await handle.close()
  }
}

async function writeNewFileDurable(path: string, text: string): Promise<void> {
  const handle = await open(path, 'wx', 0o600)
  try {
    await handle.writeFile(text, 'utf8')
    await handle.sync()
  } finally {
    await handle.close()
  }
}

async function loadManifest(path: string): Promise<StrongFlowJobManifest> {
  return parseManifest(JSON.parse(await readFile(path, 'utf8')) as unknown)
}

function sameManifest(left: StrongFlowJobManifest, right: StrongFlowJobManifest): boolean {
  return left.schemaVersion === right.schemaVersion
    && left.jobId === right.jobId
    && left.createdEventId === right.createdEventId
    && left.createdAtMillis === right.createdAtMillis
}

function eventFileSequence(name: string): string | undefined {
  return EVENT_FILE_PATTERN.exec(name)?.[1]
}

function compareSequences(left: string, right: string): number {
  const leftValue = BigInt(left)
  const rightValue = BigInt(right)
  if (leftValue < rightValue) return -1
  if (leftValue > rightValue) return 1
  return 0
}

function nextSequence(sequence: string): string {
  return (BigInt(sequence) + 1n).toString()
}

function storedListEntry(stored: StrongFlowStoredJob): StrongFlowJobListEntry {
  const { manifest, snapshot } = stored
  return Object.freeze({
    manifest,
    sequence: snapshot.sequence,
    state: snapshot.state,
    definitionRevision: snapshot.definitionRevision,
    definition: immutableJson(structuredClone(snapshot.definition)),
    approved: snapshot.approval?.payload.decision === 'approved',
    ...(snapshot.candidateId === undefined ? {} : { candidateId: snapshot.candidateId }),
  })
}

/** One append-only StrongFlow job, stored separately from runtime and UI records. */
export class StrongFlowJobStore {
  readonly home: string
  readonly directory: string
  readonly manifestPath: string
  readonly eventsDirectory: string
  #manifest: StrongFlowJobManifest
  #tail: Promise<void> = Promise.resolve()

  private constructor(home: string, directory: string, manifest: StrongFlowJobManifest) {
    this.home = home
    this.directory = directory
    this.manifestPath = join(directory, 'manifest.json')
    this.eventsDirectory = join(directory, 'events')
    this.#manifest = manifest
  }

  static async create(options: CreateStrongFlowJobStoreOptions): Promise<StrongFlowJobStore> {
    const home = validateHome(options.home)
    assertStrongFlowJobEvent(options.event)
    applyStrongFlowJobEvent(undefined, options.event)
    const manifest = manifestFrom(options.event)
    const root = jobsRoot(home)
    const directory = jobDirectory(home, manifest.jobId)
    await mkdir(root, { recursive: true, mode: 0o700 })
    if (await pathExists(directory)) {
      throw new StrongFlowJobStoreError(
        'JOB_ALREADY_EXISTS',
        `StrongFlow job ${manifest.jobId} already exists`,
      )
    }

    const temporary = join(root, `.creating-${jobKey(manifest.jobId)}-${randomUUID()}`)
    try {
      await mkdir(temporary, { mode: 0o700 })
      const temporaryEvents = join(temporary, 'events')
      await mkdir(temporaryEvents, { mode: 0o700 })
      await writeNewFileDurable(
        join(temporary, 'manifest.json'),
        `${JSON.stringify(manifest, null, 2)}\n`,
      )
      await writeNewFileDurable(
        join(temporaryEvents, `${options.event.sequence}.json`),
        `${JSON.stringify(options.event)}\n`,
      )
      await syncDirectory(temporaryEvents)
      await syncDirectory(temporary)
      if (await pathExists(directory)) {
        throw new StrongFlowJobStoreError(
          'JOB_ALREADY_EXISTS',
          `StrongFlow job ${manifest.jobId} already exists`,
        )
      }
      await rename(temporary, directory)
      await syncDirectory(root)
      return new StrongFlowJobStore(home, directory, manifest)
    } catch (error) {
      await rm(temporary, { recursive: true, force: true })
      if (error instanceof StrongFlowJobStoreError) throw error
      if (['EEXIST', 'ENOTEMPTY'].includes(errorCode(error) ?? '')) {
        throw new StrongFlowJobStoreError(
          'JOB_ALREADY_EXISTS',
          `StrongFlow job ${manifest.jobId} already exists`,
          { cause: error },
        )
      }
      throw new StrongFlowJobStoreError(
        'STORE_IO_ERROR',
        `StrongFlow job ${manifest.jobId} could not be created`,
        { cause: error },
      )
    }
  }

  static async open(homeInput: string, jobIdInput: string): Promise<StrongFlowJobStore> {
    const home = validateHome(homeInput)
    const jobId = validateJobId(jobIdInput)
    const directory = jobDirectory(home, jobId)
    if (!(await pathExists(directory))) {
      throw new StrongFlowJobStoreError(
        'JOB_NOT_FOUND',
        `StrongFlow job ${jobId} was not found`,
      )
    }
    try {
      if (!(await lstat(directory)).isDirectory()) {
        throw new Error('job path is not a directory')
      }
      const manifest = await loadManifest(join(directory, 'manifest.json'))
      if (manifest.jobId !== jobId || jobKey(manifest.jobId) !== jobKey(jobId)) {
        throw new Error('manifest job identity does not match its directory')
      }
      const store = new StrongFlowJobStore(home, directory, manifest)
      await store.#readUnlocked()
      return store
    } catch (error) {
      if (error instanceof StrongFlowJobStoreError) throw error
      throw new StrongFlowJobStoreError(
        'STORE_CORRUPT',
        `StrongFlow job ${jobId} is corrupt`,
        { cause: error },
      )
    }
  }

  static async list(homeInput: string): Promise<readonly StrongFlowJobListEntry[]> {
    const home = validateHome(homeInput)
    const root = jobsRoot(home)
    if (!(await pathExists(root))) return Object.freeze([])
    try {
      const entries = await readdir(root, { withFileTypes: true })
      const result: StrongFlowJobListEntry[] = []
      for (const entry of entries) {
        if (CREATING_DIRECTORY_PATTERN.test(entry.name)) continue
        if (!entry.isDirectory() || !JOB_DIRECTORY_PATTERN.test(entry.name)) {
          throw new Error(`unexpected job-store entry ${entry.name}`)
        }
        const directory = join(root, entry.name)
        const manifest = await loadManifest(join(directory, 'manifest.json'))
        if (jobKey(manifest.jobId) !== entry.name) {
          throw new Error(`job directory ${entry.name} has the wrong manifest identity`)
        }
        const store = new StrongFlowJobStore(home, directory, manifest)
        result.push(storedListEntry(await store.#readUnlocked()))
      }
      result.sort((left, right) => (
        left.manifest.createdAtMillis - right.manifest.createdAtMillis
        || left.manifest.jobId.localeCompare(right.manifest.jobId)
      ))
      return Object.freeze(result)
    } catch (error) {
      if (error instanceof StrongFlowJobStoreError) throw error
      throw new StrongFlowJobStoreError(
        'STORE_CORRUPT',
        'StrongFlow job index is corrupt',
        { cause: error },
      )
    }
  }

  get manifest(): StrongFlowJobManifest {
    return immutableJson(structuredClone(this.#manifest))
  }

  async append(event: StrongFlowJobEvent): Promise<StrongFlowJobSnapshot> {
    return this.#serialize(async () => {
      assertStrongFlowJobEvent(event)
      if (event.jobId !== this.#manifest.jobId) {
        throw new StrongFlowJobStoreError(
          'JOB_ID_MISMATCH',
          `event ${event.id} does not belong to StrongFlow job ${this.#manifest.jobId}`,
        )
      }
      const stored = await this.#readUnlocked()
      const expected = nextSequence(stored.snapshot.sequence)
      const relation = compareSequences(event.sequence, expected)
      if (relation < 0) {
        throw new StrongFlowJobStoreError(
          'EVENT_ALREADY_EXISTS',
          `event sequence ${event.sequence} is already published for job ${event.jobId}`,
        )
      }
      if (relation > 0) {
        throw new StrongFlowJobStoreError(
          'EVENT_SEQUENCE_MISMATCH',
          `event ${event.id} has sequence ${event.sequence}; expected ${expected}`,
        )
      }
      const snapshot = applyStrongFlowJobEvent(stored.snapshot, event)
      await this.#publishEvent(event)
      return snapshot
    })
  }

  async read(): Promise<StrongFlowStoredJob> {
    await this.#tail
    return this.#readUnlocked()
  }

  async #publishEvent(event: StrongFlowJobEvent): Promise<void> {
    const temporary = join(
      this.eventsDirectory,
      `.pending-${event.sequence}-${randomUUID()}.json`,
    )
    const published = join(this.eventsDirectory, `${event.sequence}.json`)
    try {
      await writeNewFileDurable(temporary, `${JSON.stringify(event)}\n`)
      await link(temporary, published)
      await syncDirectory(this.eventsDirectory)
    } catch (error) {
      await rm(temporary, { force: true })
      if (errorCode(error) === 'EEXIST') {
        throw new StrongFlowJobStoreError(
          'EVENT_ALREADY_EXISTS',
          `event sequence ${event.sequence} is already published for job ${event.jobId}`,
          { cause: error },
        )
      }
      throw new StrongFlowJobStoreError(
        'STORE_IO_ERROR',
        `event ${event.id} could not be published`,
        { cause: error },
      )
    }
    try {
      await rm(temporary, { force: true })
      await syncDirectory(this.eventsDirectory)
    } catch {
      // The durable published link is authoritative; pending links are always ignored.
    }
  }

  async #readUnlocked(): Promise<StrongFlowStoredJob> {
    try {
      const manifest = await loadManifest(this.manifestPath)
      if (
        !sameManifest(manifest, this.#manifest)
        || jobKey(manifest.jobId) !== basename(this.directory)
      ) throw new Error('manifest identity changed')

      const directoryEntries = await readdir(this.eventsDirectory, { withFileTypes: true })
      const eventFiles: { readonly name: string; readonly sequence: string }[] = []
      for (const entry of directoryEntries) {
        if (PENDING_EVENT_PATTERN.test(entry.name)) continue
        const sequence = eventFileSequence(entry.name)
        if (!entry.isFile() || sequence === undefined) {
          throw new Error(`unexpected event-store entry ${entry.name}`)
        }
        eventFiles.push({ name: entry.name, sequence })
      }
      eventFiles.sort((left, right) => compareSequences(left.sequence, right.sequence))
      if (eventFiles.length === 0) throw new Error('job has no events')

      const events: StrongFlowJobEvent[] = []
      let expected = '1'
      for (const file of eventFiles) {
        if (file.sequence !== expected) {
          throw new Error(`event sequence ${file.sequence} appears where ${expected} was expected`)
        }
        const text = await readFile(join(this.eventsDirectory, file.name), 'utf8')
        if (!text.endsWith('\n') || text.slice(0, -1).includes('\n')) {
          throw new Error(`event file ${file.name} is incomplete or has extra records`)
        }
        const value: unknown = JSON.parse(text.slice(0, -1))
        assertStrongFlowJobEvent(value)
        if (value.jobId !== manifest.jobId || value.sequence !== file.sequence) {
          throw new Error(`event file ${file.name} has the wrong identity`)
        }
        events.push(immutableJson(value))
        expected = nextSequence(expected)
      }
      if (
        events[0]?.id !== manifest.createdEventId
        || events[0].occurredAtMillis !== manifest.createdAtMillis
      ) {
        throw new Error('manifest does not identify the first event')
      }
      const snapshot = projectStrongFlowJob(events)
      return Object.freeze({
        manifest: this.manifest,
        events: Object.freeze(events),
        snapshot,
      })
    } catch (error) {
      if (error instanceof StrongFlowJobStoreError) throw error
      throw new StrongFlowJobStoreError(
        'STORE_CORRUPT',
        `StrongFlow job ${this.#manifest.jobId} is corrupt`,
        { cause: error },
      )
    }
  }

  #serialize<Result>(operation: () => Promise<Result>): Promise<Result> {
    const current = this.#tail.then(operation, operation)
    this.#tail = current.then(() => {}, () => {})
    return current
  }
}
