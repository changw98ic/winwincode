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
  DeliveryId,
  parseDelivery,
  type Delivery,
  type DeliveryId as DeliveryIdentifier,
} from '@winwincode/contracts'

export const DELIVERY_STORE_SCHEMA_VERSION = 1 as const

export const DELIVERY_MUTATION_OPERATIONS = Object.freeze([
  'delivery.created',
  'delivery.spec.updated',
  'stage.started',
  'session.bound',
  'attention.resolved',
  'verdict.submitted',
] as const)

export type DeliveryMutationOperation = typeof DELIVERY_MUTATION_OPERATIONS[number]

export type DeliveryStoreErrorCode =
  | 'INVALID_STORE_OPTIONS'
  | 'DELIVERY_ALREADY_EXISTS'
  | 'DELIVERY_NOT_FOUND'
  | 'STORE_CORRUPT'
  | 'DELIVERY_ID_MISMATCH'
  | 'REVISION_CONFLICT'
  | 'REQUEST_CONFLICT'
  | 'STORE_IO_ERROR'

export class DeliveryStoreError extends Error {
  readonly code: DeliveryStoreErrorCode

  constructor(code: DeliveryStoreErrorCode, message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'DeliveryStoreError'
    this.code = code
  }
}

export interface DeliveryStoreManifest {
  readonly schemaVersion: typeof DELIVERY_STORE_SCHEMA_VERSION
  readonly deliveryId: DeliveryIdentifier
  readonly createdAtMillis: number
  readonly firstRecordDigest: string
}

export interface DeliveryStoreRecord {
  readonly schemaVersion: typeof DELIVERY_STORE_SCHEMA_VERSION
  readonly deliveryId: DeliveryIdentifier
  readonly sequence: string
  readonly requestId: string
  readonly requestDigest: string
  readonly operation: DeliveryMutationOperation
  readonly previousDigest: string | null
  readonly snapshot: Delivery
  readonly digest: string
}

export interface CreateDeliveryStoreOptions {
  readonly home: string
  readonly requestId: string
  readonly requestDigest: string
  readonly snapshot: Delivery
}

export interface AppendDeliveryStoreOptions {
  readonly requestId: string
  readonly requestDigest: string
  readonly operation: Exclude<DeliveryMutationOperation, 'delivery.created'>
  readonly expectedRevision: number
  readonly snapshot: Delivery
}

export interface StoredDelivery {
  readonly manifest: DeliveryStoreManifest
  readonly records: readonly DeliveryStoreRecord[]
  readonly snapshot: Delivery
}

export interface DeliveryStoreMutationResult {
  readonly snapshot: Delivery
  readonly replayed: boolean
}

const DELIVERY_DIRECTORY_PATTERN = /^[a-f0-9]{64}$/u
const RECORD_FILE_PATTERN = /^([1-9][0-9]*)\.json$/u
const CREATING_DIRECTORY_PATTERN = /^\.creating-[a-f0-9]{64}-[0-9a-f-]+$/u
const PENDING_RECORD_PATTERN = /^\.pending-[1-9][0-9]*-[0-9a-f-]+\.json$/u
const DIGEST_PATTERN = /^[0-9a-f]{64}$/u
const REQUEST_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function errorCode(error: unknown): string | undefined {
  if (typeof error !== 'object' || error === null || !('code' in error)) return undefined
  return typeof error.code === 'string' ? error.code : undefined
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  expected: readonly string[],
  label: string,
): void {
  const keys = new Set(expected)
  if (Object.keys(value).length !== keys.size
    || expected.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !keys.has(key))) {
    throw new Error(`${label} has an unexpected shape`)
  }
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

function validateHome(home: string): string {
  if (typeof home !== 'string' || home.length === 0) {
    throw new DeliveryStoreError(
      'INVALID_STORE_OPTIONS',
      'StrongFlow delivery home must be a non-empty path',
    )
  }
  return resolve(home)
}

function validateDeliveryId(value: string): DeliveryIdentifier {
  try {
    return DeliveryId(value)
  } catch (error) {
    throw new DeliveryStoreError(
      'INVALID_STORE_OPTIONS',
      'StrongFlow delivery id is invalid',
      { cause: error },
    )
  }
}

function validateRequestId(value: string): string {
  if (typeof value !== 'string' || !REQUEST_ID_PATTERN.test(value)) {
    throw new DeliveryStoreError(
      'INVALID_STORE_OPTIONS',
      'StrongFlow delivery request id is invalid',
    )
  }
  return value
}

function validateDigest(value: string, label: string): string {
  if (typeof value !== 'string' || !DIGEST_PATTERN.test(value)) {
    throw new DeliveryStoreError('INVALID_STORE_OPTIONS', `${label} is invalid`)
  }
  return value
}

function nonNegativeInteger(value: unknown, label: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Object.is(value, -0)) {
    throw new Error(`${label} must be a non-negative safe integer`)
  }
  return Number(value)
}

function deliveryRoot(home: string): string {
  return join(validateHome(home), 'strongflow-deliveries')
}

function deliveryKey(deliveryId: string): string {
  return createHash('sha256').update(deliveryId).digest('hex')
}

function deliveryDirectory(home: string, deliveryId: string): string {
  return join(deliveryRoot(home), deliveryKey(deliveryId))
}

function recordPayload(record: Omit<DeliveryStoreRecord, 'digest'>): string {
  return JSON.stringify(record)
}

function recordDigest(record: Omit<DeliveryStoreRecord, 'digest'>): string {
  return createHash('sha256').update(recordPayload(record)).digest('hex')
}

function materializeRecord(input: Omit<DeliveryStoreRecord, 'digest'>): DeliveryStoreRecord {
  const parsed = Object.freeze({
    ...input,
    snapshot: parseDelivery(input.snapshot),
  })
  return Object.freeze({ ...parsed, digest: recordDigest(parsed) })
}

function manifestFrom(record: DeliveryStoreRecord): DeliveryStoreManifest {
  return Object.freeze({
    schemaVersion: DELIVERY_STORE_SCHEMA_VERSION,
    deliveryId: record.deliveryId,
    createdAtMillis: record.snapshot.createdAtMillis,
    firstRecordDigest: record.digest,
  })
}

function parseManifest(value: unknown): DeliveryStoreManifest {
  if (!isRecord(value)) throw new Error('delivery manifest must be an object')
  exactKeys(
    value,
    ['schemaVersion', 'deliveryId', 'createdAtMillis', 'firstRecordDigest'],
    'delivery manifest',
  )
  if (value.schemaVersion !== DELIVERY_STORE_SCHEMA_VERSION) {
    throw new Error('delivery manifest schemaVersion is unsupported')
  }
  if (typeof value.deliveryId !== 'string') throw new Error('manifest deliveryId is invalid')
  const deliveryId = DeliveryId(value.deliveryId)
  const createdAtMillis = nonNegativeInteger(value.createdAtMillis, 'manifest createdAtMillis')
  if (typeof value.firstRecordDigest !== 'string'
    || !DIGEST_PATTERN.test(value.firstRecordDigest)) {
    throw new Error('manifest firstRecordDigest is invalid')
  }
  return Object.freeze({
    schemaVersion: DELIVERY_STORE_SCHEMA_VERSION,
    deliveryId,
    createdAtMillis,
    firstRecordDigest: value.firstRecordDigest,
  })
}

function parseStoreRecord(value: unknown): DeliveryStoreRecord {
  if (!isRecord(value)) throw new Error('delivery record must be an object')
  exactKeys(value, [
    'schemaVersion',
    'deliveryId',
    'sequence',
    'requestId',
    'requestDigest',
    'operation',
    'previousDigest',
    'snapshot',
    'digest',
  ], 'delivery record')
  if (value.schemaVersion !== DELIVERY_STORE_SCHEMA_VERSION) {
    throw new Error('delivery record schemaVersion is unsupported')
  }
  if (typeof value.deliveryId !== 'string') throw new Error('record deliveryId is invalid')
  const deliveryId = DeliveryId(value.deliveryId)
  if (typeof value.sequence !== 'string' || !/^[1-9][0-9]*$/u.test(value.sequence)) {
    throw new Error('record sequence is invalid')
  }
  if (typeof value.requestId !== 'string' || !REQUEST_ID_PATTERN.test(value.requestId)) {
    throw new Error('record requestId is invalid')
  }
  if (typeof value.requestDigest !== 'string' || !DIGEST_PATTERN.test(value.requestDigest)) {
    throw new Error('record requestDigest is invalid')
  }
  if (typeof value.operation !== 'string'
    || !DELIVERY_MUTATION_OPERATIONS.includes(value.operation as DeliveryMutationOperation)) {
    throw new Error('record operation is invalid')
  }
  if (value.previousDigest !== null
    && (typeof value.previousDigest !== 'string' || !DIGEST_PATTERN.test(value.previousDigest))) {
    throw new Error('record previousDigest is invalid')
  }
  if (typeof value.digest !== 'string' || !DIGEST_PATTERN.test(value.digest)) {
    throw new Error('record digest is invalid')
  }
  const snapshot = parseDelivery(value.snapshot)
  const parsed = Object.freeze({
    schemaVersion: DELIVERY_STORE_SCHEMA_VERSION,
    deliveryId,
    sequence: value.sequence,
    requestId: value.requestId,
    requestDigest: value.requestDigest,
    operation: value.operation as DeliveryMutationOperation,
    previousDigest: value.previousDigest,
    snapshot,
  })
  if (recordDigest(parsed) !== value.digest) throw new Error('delivery record digest changed')
  return Object.freeze({ ...parsed, digest: value.digest })
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

async function loadManifest(path: string): Promise<DeliveryStoreManifest> {
  return parseManifest(JSON.parse(await readFile(path, 'utf8')) as unknown)
}

function sameManifest(left: DeliveryStoreManifest, right: DeliveryStoreManifest): boolean {
  return left.schemaVersion === right.schemaVersion
    && left.deliveryId === right.deliveryId
    && left.createdAtMillis === right.createdAtMillis
    && left.firstRecordDigest === right.firstRecordDigest
}

function recordFileSequence(name: string): string | undefined {
  return RECORD_FILE_PATTERN.exec(name)?.[1]
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

/** Append-only durable owner for one canonical Delivery. */
export class DeliveryStore {
  readonly home: string
  readonly directory: string
  readonly manifestPath: string
  readonly recordsDirectory: string
  readonly #manifest: DeliveryStoreManifest
  #tail: Promise<void> = Promise.resolve()

  private constructor(home: string, directory: string, manifest: DeliveryStoreManifest) {
    this.home = home
    this.directory = directory
    this.manifestPath = join(directory, 'manifest.json')
    this.recordsDirectory = join(directory, 'records')
    this.#manifest = manifest
  }

  static async create(options: CreateDeliveryStoreOptions): Promise<DeliveryStore> {
    const home = validateHome(options.home)
    const requestId = validateRequestId(options.requestId)
    const requestDigest = validateDigest(options.requestDigest, 'requestDigest')
    const snapshot = parseDelivery(options.snapshot)
    if (snapshot.revision !== 1) {
      throw new DeliveryStoreError(
        'INVALID_STORE_OPTIONS',
        'a new StrongFlow delivery must start at revision 1',
      )
    }
    const record = materializeRecord({
      schemaVersion: DELIVERY_STORE_SCHEMA_VERSION,
      deliveryId: snapshot.id,
      sequence: '1',
      requestId,
      requestDigest,
      operation: 'delivery.created',
      previousDigest: null,
      snapshot,
    })
    const manifest = manifestFrom(record)
    const root = deliveryRoot(home)
    const directory = deliveryDirectory(home, snapshot.id)
    await mkdir(root, { recursive: true, mode: 0o700 })
    if (await pathExists(directory)) {
      throw new DeliveryStoreError(
        'DELIVERY_ALREADY_EXISTS',
        `StrongFlow delivery ${snapshot.id} already exists`,
      )
    }
    const temporary = join(root, `.creating-${deliveryKey(snapshot.id)}-${randomUUID()}`)
    try {
      await mkdir(temporary, { mode: 0o700 })
      const temporaryRecords = join(temporary, 'records')
      await mkdir(temporaryRecords, { mode: 0o700 })
      await writeNewFileDurable(
        join(temporary, 'manifest.json'),
        `${JSON.stringify(manifest, null, 2)}\n`,
      )
      await writeNewFileDurable(
        join(temporaryRecords, '1.json'),
        `${JSON.stringify(record)}\n`,
      )
      await syncDirectory(temporaryRecords)
      await syncDirectory(temporary)
      await rename(temporary, directory)
      await syncDirectory(root)
      return new DeliveryStore(home, directory, manifest)
    } catch (error) {
      await rm(temporary, { recursive: true, force: true })
      if (error instanceof DeliveryStoreError) throw error
      if (['EEXIST', 'ENOTEMPTY'].includes(errorCode(error) ?? '')) {
        throw new DeliveryStoreError(
          'DELIVERY_ALREADY_EXISTS',
          `StrongFlow delivery ${snapshot.id} already exists`,
          { cause: error },
        )
      }
      throw new DeliveryStoreError(
        'STORE_IO_ERROR',
        `StrongFlow delivery ${snapshot.id} could not be created`,
        { cause: error },
      )
    }
  }

  static async open(homeInput: string, deliveryIdInput: string): Promise<DeliveryStore> {
    const home = validateHome(homeInput)
    const deliveryId = validateDeliveryId(deliveryIdInput)
    const directory = deliveryDirectory(home, deliveryId)
    if (!(await pathExists(directory))) {
      throw new DeliveryStoreError(
        'DELIVERY_NOT_FOUND',
        `StrongFlow delivery ${deliveryId} was not found`,
      )
    }
    try {
      if (!(await lstat(directory)).isDirectory()) throw new Error('delivery path is not a directory')
      const manifest = await loadManifest(join(directory, 'manifest.json'))
      if (manifest.deliveryId !== deliveryId
        || deliveryKey(manifest.deliveryId) !== basename(directory)) {
        throw new Error('manifest delivery identity does not match its directory')
      }
      const store = new DeliveryStore(home, directory, manifest)
      await store.#readUnlocked()
      return store
    } catch (error) {
      if (error instanceof DeliveryStoreError) throw error
      throw new DeliveryStoreError(
        'STORE_CORRUPT',
        `StrongFlow delivery ${deliveryId} is corrupt`,
        { cause: error },
      )
    }
  }

  get manifest(): DeliveryStoreManifest {
    return immutableJson(structuredClone(this.#manifest))
  }

  async read(): Promise<StoredDelivery> {
    await this.#tail
    return this.#readUnlocked()
  }

  async append(options: AppendDeliveryStoreOptions): Promise<DeliveryStoreMutationResult> {
    return this.#serialize(async () => {
      const requestId = validateRequestId(options.requestId)
      const requestDigest = validateDigest(options.requestDigest, 'requestDigest')
      const snapshot = parseDelivery(options.snapshot)
      if (snapshot.id !== this.#manifest.deliveryId) {
        throw new DeliveryStoreError(
          'DELIVERY_ID_MISMATCH',
          'delivery mutation snapshot belongs to another delivery',
        )
      }
      const stored = await this.#readUnlocked()
      const prior = stored.records.find(record => record.requestId === requestId)
      if (prior !== undefined) {
        if (prior.requestDigest !== requestDigest || prior.operation !== options.operation) {
          throw new DeliveryStoreError(
            'REQUEST_CONFLICT',
            `request ${requestId} was already used for another delivery mutation`,
          )
        }
        return Object.freeze({ snapshot: prior.snapshot, replayed: true })
      }
      if (options.expectedRevision !== stored.snapshot.revision
        || snapshot.revision !== stored.snapshot.revision + 1) {
        throw new DeliveryStoreError(
          'REVISION_CONFLICT',
          `delivery ${snapshot.id} revision changed before mutation ${requestId}`,
        )
      }
      const previous = stored.records.at(-1)!
      const sequence = nextSequence(previous.sequence)
      if (snapshot.revision !== Number(sequence)) {
        throw new DeliveryStoreError(
          'REVISION_CONFLICT',
          'delivery revision and durable sequence diverged',
        )
      }
      const record = materializeRecord({
        schemaVersion: DELIVERY_STORE_SCHEMA_VERSION,
        deliveryId: snapshot.id,
        sequence,
        requestId,
        requestDigest,
        operation: options.operation,
        previousDigest: previous.digest,
        snapshot,
      })
      try {
        await this.#publishRecord(record)
        return Object.freeze({ snapshot: record.snapshot, replayed: false })
      } catch (error) {
        if (!(error instanceof DeliveryStoreError) || error.code !== 'REVISION_CONFLICT') {
          throw error
        }
        const raced = await this.#readUnlocked()
        const replay = raced.records.find(entry => entry.requestId === requestId)
        if (replay !== undefined
          && replay.requestDigest === requestDigest
          && replay.operation === options.operation) {
          return Object.freeze({ snapshot: replay.snapshot, replayed: true })
        }
        throw error
      }
    })
  }

  async #publishRecord(record: DeliveryStoreRecord): Promise<void> {
    const temporary = join(
      this.recordsDirectory,
      `.pending-${record.sequence}-${randomUUID()}.json`,
    )
    const published = join(this.recordsDirectory, `${record.sequence}.json`)
    try {
      await writeNewFileDurable(temporary, `${JSON.stringify(record)}\n`)
      await link(temporary, published)
      await syncDirectory(this.recordsDirectory)
    } catch (error) {
      await rm(temporary, { force: true })
      if (errorCode(error) === 'EEXIST') {
        throw new DeliveryStoreError(
          'REVISION_CONFLICT',
          `delivery revision ${record.sequence} was already published`,
          { cause: error },
        )
      }
      throw new DeliveryStoreError(
        'STORE_IO_ERROR',
        `delivery mutation ${record.requestId} could not be published`,
        { cause: error },
      )
    }
    try {
      await rm(temporary, { force: true })
      await syncDirectory(this.recordsDirectory)
    } catch {
      // The published hard link is authoritative; pending files are ignored.
    }
  }

  async #readUnlocked(): Promise<StoredDelivery> {
    try {
      const manifest = await loadManifest(this.manifestPath)
      if (!sameManifest(manifest, this.#manifest)
        || deliveryKey(manifest.deliveryId) !== basename(this.directory)) {
        throw new Error('delivery manifest identity changed')
      }
      const entries = await readdir(this.recordsDirectory, { withFileTypes: true })
      const files: { readonly name: string; readonly sequence: string }[] = []
      for (const entry of entries) {
        if (PENDING_RECORD_PATTERN.test(entry.name)) continue
        const sequence = recordFileSequence(entry.name)
        if (!entry.isFile() || sequence === undefined) {
          throw new Error(`unexpected delivery-store entry ${entry.name}`)
        }
        files.push({ name: entry.name, sequence })
      }
      files.sort((left, right) => compareSequences(left.sequence, right.sequence))
      if (files.length === 0) throw new Error('delivery has no records')

      const records: DeliveryStoreRecord[] = []
      let expected = '1'
      let previousDigest: string | null = null
      for (const file of files) {
        if (file.sequence !== expected) {
          throw new Error(`delivery sequence ${file.sequence} appears where ${expected} was expected`)
        }
        const text = await readFile(join(this.recordsDirectory, file.name), 'utf8')
        if (!text.endsWith('\n') || text.slice(0, -1).includes('\n')) {
          throw new Error(`delivery record ${file.name} is incomplete or has extra records`)
        }
        const record = parseStoreRecord(JSON.parse(text.slice(0, -1)) as unknown)
        if (record.deliveryId !== manifest.deliveryId
          || record.sequence !== file.sequence
          || record.previousDigest !== previousDigest
          || record.snapshot.revision !== Number(file.sequence)
          || record.snapshot.id !== manifest.deliveryId) {
          throw new Error(`delivery record ${file.name} has a broken relationship`)
        }
        if (records.some(entry => entry.requestId === record.requestId)) {
          throw new Error(`delivery request ${record.requestId} is duplicated`)
        }
        records.push(record)
        previousDigest = record.digest
        expected = nextSequence(expected)
      }
      if (records[0]?.operation !== 'delivery.created'
        || records[0].digest !== manifest.firstRecordDigest
        || records[0].snapshot.createdAtMillis !== manifest.createdAtMillis) {
        throw new Error('delivery manifest does not identify its first record')
      }
      return Object.freeze({
        manifest: this.manifest,
        records: Object.freeze(records),
        snapshot: records.at(-1)!.snapshot,
      })
    } catch (error) {
      if (error instanceof DeliveryStoreError) throw error
      throw new DeliveryStoreError(
        'STORE_CORRUPT',
        `StrongFlow delivery ${this.#manifest.deliveryId} is corrupt`,
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
