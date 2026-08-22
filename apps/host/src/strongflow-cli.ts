import { randomUUID } from 'node:crypto'
import { readFile } from 'node:fs/promises'

import {
  STRONGFLOW_ARTIFACT_KINDS,
  STRONGFLOW_CLI_COMMANDS,
  STRONGFLOW_CLI_EXIT_CODES,
  STRONGFLOW_OPERATOR_SCHEMA_VERSION,
  materializeStrongFlowOperatorFailure,
  parseStrongFlowOperatorRequest,
  parseStrongFlowOperatorResponse,
  parseStrongFlowOperatorResponseForRequest,
  renderStrongFlowCliHelp,
  strongFlowCliExitCode,
  strongFlowCliSignalExitCode,
  type DefinitionIdentity,
  type StrongFlowCliSignal,
  type StrongFlowOperatorInvoker,
  type StrongFlowOperatorOperation,
  type StrongFlowOperatorRequest,
  type StrongFlowOperatorResponse,
} from '@winwincode/contracts'

export interface StrongFlowCliIo {
  readonly stdout: (text: string) => void
  readonly stderr: (text: string) => void
  readonly readTextFile: (path: string) => Promise<string>
  readonly requestId: () => string
  readonly signal?: AbortSignal
  readonly interruptedBy?: () => StrongFlowCliSignal | undefined
}

interface ParsedArguments {
  readonly positionals: readonly string[]
  readonly flags: ReadonlyMap<string, readonly string[]>
}

class CliUsageError extends Error {
  readonly operation: StrongFlowOperatorOperation | null
  readonly requestId: string | null

  constructor(
    message: string,
    options: {
      readonly operation?: StrongFlowOperatorOperation | null
      readonly requestId?: string | null
    } = {},
  ) {
    super(message)
    this.name = 'CliUsageError'
    this.operation = options.operation ?? null
    this.requestId = options.requestId ?? null
  }
}

const DEFAULT_IO: StrongFlowCliIo = Object.freeze({
  stdout: (text: string) => process.stdout.write(text),
  stderr: (text: string) => process.stderr.write(text),
  readTextFile: (path: string) => readFile(path, 'utf8'),
  requestId: () => `cli-${randomUUID()}`,
})

const BOOLEAN_FLAGS = new Set(['json', 'json-lines'])

function parseArguments(tokens: readonly string[]): ParsedArguments {
  const positionals: string[] = []
  const flags = new Map<string, string[]>()
  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index]!
    if (!token.startsWith('--')) {
      positionals.push(token)
      continue
    }
    const separator = token.indexOf('=')
    const name = token.slice(2, separator === -1 ? undefined : separator)
    if (name.length === 0) throw new CliUsageError('选项名称不能为空。')
    let value: string
    if (separator !== -1) {
      value = token.slice(separator + 1)
    } else if (BOOLEAN_FLAGS.has(name)) {
      value = 'true'
    } else {
      const next = tokens[index + 1]
      if (next === undefined || next.startsWith('--')) {
        throw new CliUsageError(`--${name} 缺少值。`)
      }
      value = next
      index += 1
    }
    const entries = flags.get(name) ?? []
    entries.push(value)
    flags.set(name, entries)
  }
  return Object.freeze({ positionals: Object.freeze(positionals), flags })
}

function commandOperation(command: string): StrongFlowOperatorOperation | null {
  return STRONGFLOW_CLI_COMMANDS.find(entry => entry.command === command)?.operation ?? null
}

function onlyFlag(
  parsed: ParsedArguments,
  name: string,
  options: { readonly required?: boolean } = {},
): string | undefined {
  const values = parsed.flags.get(name) ?? []
  if (values.length > 1) throw new CliUsageError(`--${name} 不能重复。`)
  if (options.required === true && values.length === 0) {
    throw new CliUsageError(`缺少 --${name}。`)
  }
  return values[0]
}

function requireOutputFlag(parsed: ParsedArguments, expected: 'json' | 'json-lines'): void {
  if (onlyFlag(parsed, expected) !== 'true') {
    throw new CliUsageError(`该命令要求 --${expected}。`)
  }
  const other = expected === 'json' ? 'json-lines' : 'json'
  if (parsed.flags.has(other)) throw new CliUsageError(`--${other} 不适用于该命令。`)
}

function rejectUnknownFlags(parsed: ParsedArguments, allowed: readonly string[]): void {
  const accepted = new Set([...allowed, 'request-id'])
  const unknown = [...parsed.flags.keys()].find(name => !accepted.has(name))
  if (unknown !== undefined) throw new CliUsageError(`未知选项 --${unknown}。`)
}

function exactPositionals(parsed: ParsedArguments, count: number): void {
  if (parsed.positionals.length !== count) {
    throw new CliUsageError(`该命令需要 ${String(count)} 个位置参数。`)
  }
}

function positiveInteger(value: string | undefined, fallback: number, name: string): number {
  if (value === undefined) return fallback
  if (!/^[1-9][0-9]*$/u.test(value)) throw new CliUsageError(`--${name} 必须是正整数。`)
  const parsed = Number(value)
  if (!Number.isSafeInteger(parsed)) throw new CliUsageError(`--${name} 数值过大。`)
  return parsed
}

function nonNegativeInteger(value: string | undefined, fallback: number, name: string): number {
  if (value === undefined) return fallback
  if (!/^(?:0|[1-9][0-9]*)$/u.test(value)) {
    throw new CliUsageError(`--${name} 必须是非负整数。`)
  }
  const parsed = Number(value)
  if (!Number.isSafeInteger(parsed)) throw new CliUsageError(`--${name} 数值过大。`)
  return parsed
}

async function definitionFromFile(
  path: string,
  io: StrongFlowCliIo,
): Promise<DefinitionIdentity> {
  let value: unknown
  try {
    value = JSON.parse(await io.readTextFile(path)) as unknown
  } catch {
    throw new CliUsageError('定义文件不是可读取的 JSON。')
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new CliUsageError('定义文件必须直接包含四个定义身份。')
  }
  return value as DefinitionIdentity
}

function operatorRequest(
  operation: StrongFlowOperatorOperation,
  requestId: string,
  payload: unknown,
): StrongFlowOperatorRequest {
  return parseStrongFlowOperatorRequest({
    schemaVersion: STRONGFLOW_OPERATOR_SCHEMA_VERSION,
    requestId,
    operation,
    payload,
  })
}

async function buildRequest(
  command: string,
  parsed: ParsedArguments,
  io: StrongFlowCliIo,
): Promise<StrongFlowOperatorRequest> {
  const operation = commandOperation(command)
  if (operation === null) throw new CliUsageError(`未知命令 ${command}。`)
  const requestId = onlyFlag(parsed, 'request-id') ?? io.requestId()
  try {
    switch (operation) {
      case 'job.create': {
        rejectUnknownFlags(parsed, ['repo', 'request', 'base', 'title', 'json'])
        requireOutputFlag(parsed, 'json')
        exactPositionals(parsed, 0)
        return operatorRequest(operation, requestId, {
          repositoryPath: onlyFlag(parsed, 'repo', { required: true })!,
          baseRevision: onlyFlag(parsed, 'base') ?? null,
          title: onlyFlag(parsed, 'title') ?? null,
          request: onlyFlag(parsed, 'request', { required: true })!,
          submittedFrom: 'cli',
        })
      }
      case 'job.status':
      case 'definition.requirement':
      case 'definition.solution':
      case 'definition.diagrams': {
        rejectUnknownFlags(parsed, ['json'])
        requireOutputFlag(parsed, 'json')
        exactPositionals(parsed, 1)
        return operatorRequest(operation, requestId, {
          jobId: parsed.positionals[0]!,
        })
      }
      case 'job.follow': {
        rejectUnknownFlags(parsed, ['after', 'limit', 'wait', 'json-lines'])
        requireOutputFlag(parsed, 'json-lines')
        exactPositionals(parsed, 1)
        return operatorRequest(operation, requestId, {
          jobId: parsed.positionals[0]!,
          afterCursor: onlyFlag(parsed, 'after') ?? null,
          limit: positiveInteger(onlyFlag(parsed, 'limit'), 100, 'limit'),
          waitMillis: nonNegativeInteger(onlyFlag(parsed, 'wait'), 0, 'wait'),
        })
      }
      case 'review.approve':
      case 'review.reject':
      case 'review.request-changes': {
        const allowed = operation === 'review.request-changes'
          ? ['definition', 'scope', 'auth', 'comment', 'json']
          : ['definition', 'auth', 'comment', 'json']
        rejectUnknownFlags(parsed, allowed)
        requireOutputFlag(parsed, 'json')
        exactPositionals(parsed, 1)
        const definitionPath = onlyFlag(parsed, 'definition', { required: true })!
        const common = {
          jobId: parsed.positionals[0]!,
          definition: await definitionFromFile(definitionPath, io),
          channel: 'cli' as const,
          authentication: {
            scheme: 'local-peer' as const,
            proof: onlyFlag(parsed, 'auth', { required: true })!,
          },
          comment: onlyFlag(parsed, 'comment') ?? null,
        }
        if (operation === 'review.request-changes') {
          return operatorRequest(operation, requestId, {
            ...common,
            scope: onlyFlag(parsed, 'scope', { required: true }) as (
              'requirements' | 'solution' | 'diagrams'
            ),
          })
        }
        return operatorRequest(operation, requestId, common)
      }
      case 'job.cancel': {
        rejectUnknownFlags(parsed, ['reason', 'json'])
        requireOutputFlag(parsed, 'json')
        exactPositionals(parsed, 1)
        return operatorRequest(operation, requestId, {
          jobId: parsed.positionals[0]!,
          reason: onlyFlag(parsed, 'reason', { required: true })!,
        })
      }
      case 'job.resume': {
        rejectUnknownFlags(parsed, ['interruption-sequence', 'json'])
        requireOutputFlag(parsed, 'json')
        exactPositionals(parsed, 1)
        return operatorRequest(operation, requestId, {
          jobId: parsed.positionals[0]!,
          interruptionSequence: onlyFlag(
            parsed,
            'interruption-sequence',
            { required: true },
          )!,
        })
      }
      case 'job.artifacts': {
        rejectUnknownFlags(parsed, ['after-sequence', 'limit', 'kind', 'json'])
        requireOutputFlag(parsed, 'json')
        exactPositionals(parsed, 1)
        const kinds = parsed.flags.get('kind') ?? []
        return operatorRequest(operation, requestId, {
          jobId: parsed.positionals[0]!,
          afterSequence: onlyFlag(parsed, 'after-sequence') ?? null,
          limit: positiveInteger(onlyFlag(parsed, 'limit'), 100, 'limit'),
          artifactKinds: kinds.length === 0 ? STRONGFLOW_ARTIFACT_KINDS : kinds,
        })
      }
      case 'job.export': {
        rejectUnknownFlags(parsed, ['format', 'json'])
        requireOutputFlag(parsed, 'json')
        exactPositionals(parsed, 1)
        const format = onlyFlag(parsed, 'format', { required: true })
        return operatorRequest(operation, requestId, {
          jobId: parsed.positionals[0]!,
          format: format as 'manifest-json',
        })
      }
    }
  } catch (error) {
    if (error instanceof CliUsageError) {
      throw new CliUsageError(error.message, { operation, requestId })
    }
    throw new CliUsageError('命令参数不符合 StrongFlow 请求格式。', {
      operation,
      requestId,
    })
  }
}

function writeResponse(io: StrongFlowCliIo, response: StrongFlowOperatorResponse): void {
  const serialized = `${JSON.stringify(response)}\n`
  if (response.ok) io.stdout(serialized)
  else io.stderr(serialized)
}

function safeRequestId(value: string | null): string | null {
  return value !== null && /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u.test(value)
    ? value
    : null
}

/** Run one StrongFlow CLI request through the same invoker used by the DSH workbench. */
export async function runStrongFlowCli(
  argv: readonly string[],
  invoker: StrongFlowOperatorInvoker,
  overrides: Partial<StrongFlowCliIo> = {},
): Promise<number> {
  const io: StrongFlowCliIo = { ...DEFAULT_IO, ...overrides }
  const command = argv[0]
  if (command === undefined || command === 'help' || command === '--help' || command === '-h') {
    io.stdout(renderStrongFlowCliHelp())
    return STRONGFLOW_CLI_EXIT_CODES.success
  }
  let request: StrongFlowOperatorRequest
  try {
    const parsed = parseArguments(argv.slice(1))
    request = await buildRequest(command, parsed, io)
  } catch (error) {
    const usage = error instanceof CliUsageError
      ? error
      : new CliUsageError('命令参数无效。')
    const response = materializeStrongFlowOperatorFailure({
      requestId: safeRequestId(usage.requestId),
      operation: usage.operation,
      code: 'INVALID_REQUEST',
      message: usage.message,
    })
    writeResponse(io, response)
    return STRONGFLOW_CLI_EXIT_CODES.usage
  }
  let response: StrongFlowOperatorResponse
  try {
    response = parseStrongFlowOperatorResponse(
      parseStrongFlowOperatorResponseForRequest(
        request,
        await invoker.invoke(request, io.signal === undefined ? {} : { signal: io.signal }),
      ),
    )
  } catch {
    response = materializeStrongFlowOperatorFailure({
      requestId: request.requestId,
      operation: request.operation,
      code: io.signal?.aborted === true ? 'OPERATION_ABORTED' : 'INTERNAL_ERROR',
      message: io.signal?.aborted === true
        ? 'StrongFlow 操作请求已中止。'
        : 'StrongFlow 本地调用失败。',
    })
  }
  writeResponse(io, response)
  const interruptedBy = io.interruptedBy?.()
  if (!response.ok
    && response.error.code === 'OPERATION_ABORTED'
    && interruptedBy !== undefined) {
    return strongFlowCliSignalExitCode(interruptedBy)
  }
  return strongFlowCliExitCode(response)
}
