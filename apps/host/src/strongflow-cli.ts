import { randomUUID } from 'node:crypto'
import { readFile } from 'node:fs/promises'

import {
  STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
  materializeStrongFlowDeliveryFailure,
  parseStrongFlowDeliveryRequest,
  parseStrongFlowDeliveryResponseForRequest,
  type StrongFlowDeliveryInvoker,
  type StrongFlowDeliveryOperation,
  type StrongFlowDeliveryRequest,
  type StrongFlowDeliveryResponse,
} from '@winwincode/contracts'

export type StrongFlowCliSignal = 'SIGINT' | 'SIGTERM'

export const STRONGFLOW_DELIVERY_CLI_EXIT_CODES = Object.freeze({
  success: 0,
  usage: 2,
  notFound: 3,
  conflict: 4,
  service: 5,
  sigint: 130,
  sigterm: 143,
} as const)

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
  readonly operation: StrongFlowDeliveryOperation | null
  readonly requestId: string | null

  constructor(
    message: string,
    options: {
      readonly operation?: StrongFlowDeliveryOperation | null
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

const BOOLEAN_FLAGS = new Set(['json'])

const ACTION_OPERATIONS = Object.freeze({
  create: 'createDelivery',
  'update-spec': 'updateDeliverySpec',
  'start-stage': 'startStage',
  'bind-session': 'bindSession',
  'resolve-attention': 'resolveAttention',
  'submit-verdict': 'submitVerdict',
  show: 'getDeliveryProjection',
} as const satisfies Readonly<Record<string, StrongFlowDeliveryOperation>>)

export function renderStrongFlowDeliveryCliHelp(): string {
  return [
    'WinWinCode Delivery commands:',
    '  winwincode delivery create --spec FILE [--tasks FILE] --json',
    '  winwincode delivery update-spec DELIVERY_ID --expected-revision N --spec FILE --json',
    '  winwincode delivery start-stage DELIVERY_ID --expected-revision N --stage-run-id ID --stage STAGE --actor codex|human --role ROLE [--task-id ID] [--attention FILE] --json',
    '  winwincode delivery bind-session DELIVERY_ID --expected-revision N --binding-id ID --stage-run-id ID [--dsh-session ID] [--codex-session ID] --json',
    '  winwincode delivery resolve-attention DELIVERY_ID --expected-revision N --attention-id ID --decision resolved|dismissed --resolution TEXT [--remediation FILE] --auth PROOF --json',
    '  winwincode delivery submit-verdict DELIVERY_ID --expected-revision N --candidate FILE --runtime-events FILE [--required-roles reviewer,verifier] --json',
    '  winwincode delivery show DELIVERY_ID --json',
    '',
    'Every command accepts --request-id ID. FILE arguments contain canonical JSON values.',
    '',
  ].join('\n')
}

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

function requireJson(parsed: ParsedArguments): void {
  if (onlyFlag(parsed, 'json') !== 'true') {
    throw new CliUsageError('该命令要求 --json。')
  }
}

function rejectUnknownFlags(parsed: ParsedArguments, allowed: readonly string[]): void {
  const accepted = new Set([...allowed, 'request-id', 'json'])
  const unknown = [...parsed.flags.keys()].find(name => !accepted.has(name))
  if (unknown !== undefined) throw new CliUsageError(`未知选项 --${unknown}。`)
}

function exactPositionals(parsed: ParsedArguments, count: number): void {
  if (parsed.positionals.length !== count) {
    throw new CliUsageError(`该命令需要 ${String(count)} 个位置参数。`)
  }
}

function positiveInteger(value: string | undefined, name: string): number {
  if (value === undefined || !/^[1-9][0-9]*$/u.test(value)) {
    throw new CliUsageError(`--${name} 必须是正整数。`)
  }
  const parsed = Number(value)
  if (!Number.isSafeInteger(parsed)) throw new CliUsageError(`--${name} 数值过大。`)
  return parsed
}

async function readJson(path: string, label: string, io: StrongFlowCliIo): Promise<unknown> {
  try {
    return JSON.parse(await io.readTextFile(path)) as unknown
  } catch {
    throw new CliUsageError(`${label}不是可读取的 JSON。`)
  }
}

function request(
  operation: StrongFlowDeliveryOperation,
  requestId: string,
  payload: unknown,
): StrongFlowDeliveryRequest {
  return parseStrongFlowDeliveryRequest({
    schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
    requestId,
    operation,
    payload,
  })
}

async function buildRequest(
  action: string,
  parsed: ParsedArguments,
  io: StrongFlowCliIo,
): Promise<StrongFlowDeliveryRequest> {
  const operation = ACTION_OPERATIONS[action as keyof typeof ACTION_OPERATIONS] ?? null
  if (operation === null) throw new CliUsageError(`未知 Delivery 命令 ${action}。`)
  const requestId = onlyFlag(parsed, 'request-id') ?? io.requestId()
  try {
    requireJson(parsed)
    switch (operation) {
      case 'createDelivery': {
        rejectUnknownFlags(parsed, ['spec', 'tasks'])
        exactPositionals(parsed, 0)
        const tasksPath = onlyFlag(parsed, 'tasks')
        return request(operation, requestId, {
          spec: await readJson(onlyFlag(parsed, 'spec', { required: true })!, '规格文件', io),
          tasks: tasksPath === undefined ? [] : await readJson(tasksPath, '任务文件', io),
        })
      }
      case 'updateDeliverySpec': {
        rejectUnknownFlags(parsed, ['expected-revision', 'spec'])
        exactPositionals(parsed, 1)
        return request(operation, requestId, {
          deliveryId: parsed.positionals[0],
          expectedRevision: positiveInteger(
            onlyFlag(parsed, 'expected-revision', { required: true }),
            'expected-revision',
          ),
          spec: await readJson(onlyFlag(parsed, 'spec', { required: true })!, '规格文件', io),
        })
      }
      case 'startStage': {
        rejectUnknownFlags(parsed, [
          'expected-revision',
          'stage-run-id',
          'task-id',
          'stage',
          'actor',
          'role',
          'attention',
        ])
        exactPositionals(parsed, 1)
        const attentionPath = onlyFlag(parsed, 'attention')
        return request(operation, requestId, {
          deliveryId: parsed.positionals[0],
          expectedRevision: positiveInteger(
            onlyFlag(parsed, 'expected-revision', { required: true }),
            'expected-revision',
          ),
          stageRunId: onlyFlag(parsed, 'stage-run-id', { required: true }),
          deliveryTaskId: onlyFlag(parsed, 'task-id') ?? null,
          stage: onlyFlag(parsed, 'stage', { required: true }),
          actorType: onlyFlag(parsed, 'actor', { required: true }),
          role: onlyFlag(parsed, 'role', { required: true }),
          attention: attentionPath === undefined
            ? null
            : await readJson(attentionPath, '待处理事项文件', io),
        })
      }
      case 'bindSession': {
        rejectUnknownFlags(parsed, [
          'expected-revision',
          'binding-id',
          'stage-run-id',
          'dsh-session',
          'codex-session',
        ])
        exactPositionals(parsed, 1)
        return request(operation, requestId, {
          deliveryId: parsed.positionals[0],
          expectedRevision: positiveInteger(
            onlyFlag(parsed, 'expected-revision', { required: true }),
            'expected-revision',
          ),
          bindingId: onlyFlag(parsed, 'binding-id', { required: true }),
          stageRunId: onlyFlag(parsed, 'stage-run-id', { required: true }),
          dshSessionId: onlyFlag(parsed, 'dsh-session') ?? null,
          codexSessionId: onlyFlag(parsed, 'codex-session') ?? null,
        })
      }
      case 'resolveAttention': {
        rejectUnknownFlags(parsed, [
          'expected-revision',
          'attention-id',
          'decision',
          'resolution',
          'remediation',
          'auth',
        ])
        exactPositionals(parsed, 1)
        const remediationPath = onlyFlag(parsed, 'remediation')
        return request(operation, requestId, {
          deliveryId: parsed.positionals[0],
          expectedRevision: positiveInteger(
            onlyFlag(parsed, 'expected-revision', { required: true }),
            'expected-revision',
          ),
          attentionItemId: onlyFlag(parsed, 'attention-id', { required: true }),
          status: onlyFlag(parsed, 'decision', { required: true }),
          resolution: onlyFlag(parsed, 'resolution', { required: true }),
          remediation: remediationPath === undefined
            ? null
            : await readJson(remediationPath, '返工标注文件', io),
          channel: 'cli',
          authentication: {
            scheme: 'local-peer',
            proof: onlyFlag(parsed, 'auth', { required: true }),
          },
        })
      }
      case 'submitVerdict': {
        rejectUnknownFlags(parsed, [
          'expected-revision',
          'candidate',
          'runtime-events',
          'required-roles',
        ])
        exactPositionals(parsed, 1)
        return request(operation, requestId, {
          deliveryId: parsed.positionals[0],
          expectedRevision: positiveInteger(
            onlyFlag(parsed, 'expected-revision', { required: true }),
            'expected-revision',
          ),
          candidate: await readJson(
            onlyFlag(parsed, 'candidate', { required: true })!,
            '候选版本文件',
            io,
          ),
          runtimeEvents: await readJson(
            onlyFlag(parsed, 'runtime-events', { required: true })!,
            '运行事件文件',
            io,
          ),
          requiredRoles: (onlyFlag(parsed, 'required-roles') ?? 'reviewer,verifier')
            .split(',')
            .map(role => role.trim()),
        })
      }
      case 'getDeliveryProjection': {
        rejectUnknownFlags(parsed, [])
        exactPositionals(parsed, 1)
        return request(operation, requestId, {
          deliveryId: parsed.positionals[0],
        })
      }
    }
  } catch (error) {
    if (error instanceof CliUsageError) {
      throw new CliUsageError(error.message, { operation, requestId })
    }
    throw new CliUsageError('命令参数不符合 Delivery 请求格式。', {
      operation,
      requestId,
    })
  }
}

function writeResponse(io: StrongFlowCliIo, response: StrongFlowDeliveryResponse): void {
  const serialized = `${JSON.stringify(response)}\n`
  if (response.ok) io.stdout(serialized)
  else io.stderr(serialized)
}

function safeRequestId(value: string | null): string | null {
  return value !== null && /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u.test(value)
    ? value
    : null
}

function responseExitCode(response: StrongFlowDeliveryResponse): number {
  if (response.ok) return STRONGFLOW_DELIVERY_CLI_EXIT_CODES.success
  switch (response.error.code) {
    case 'INVALID_REQUEST': return STRONGFLOW_DELIVERY_CLI_EXIT_CODES.usage
    case 'DELIVERY_NOT_FOUND': return STRONGFLOW_DELIVERY_CLI_EXIT_CODES.notFound
    case 'DELIVERY_CONFLICT':
    case 'REVISION_CONFLICT':
    case 'WRONG_DELIVERY_STATE':
    case 'ATTENTION_REQUIRED': return STRONGFLOW_DELIVERY_CLI_EXIT_CODES.conflict
    default: return STRONGFLOW_DELIVERY_CLI_EXIT_CODES.service
  }
}

/** Run one Delivery request through the same invoker used by the DSH workbench. */
export async function runStrongFlowCli(
  argv: readonly string[],
  invoker: StrongFlowDeliveryInvoker,
  overrides: Partial<StrongFlowCliIo> = {},
): Promise<number> {
  const io: StrongFlowCliIo = { ...DEFAULT_IO, ...overrides }
  if (argv[0] === undefined
    || argv[0] === 'help'
    || argv[0] === '--help'
    || argv[0] === '-h'
    || (argv[0] === 'delivery' && (argv[1] === undefined || argv[1] === 'help'))) {
    io.stdout(renderStrongFlowDeliveryCliHelp())
    return STRONGFLOW_DELIVERY_CLI_EXIT_CODES.success
  }
  let requestValue: StrongFlowDeliveryRequest
  try {
    if (argv[0] !== 'delivery' || argv[1] === undefined) {
      throw new CliUsageError('命令必须以 delivery 开头。')
    }
    requestValue = await buildRequest(argv[1], parseArguments(argv.slice(2)), io)
  } catch (error) {
    const usage = error instanceof CliUsageError
      ? error
      : new CliUsageError('命令参数无效。')
    const response = materializeStrongFlowDeliveryFailure({
      requestId: safeRequestId(usage.requestId),
      operation: usage.operation,
      code: 'INVALID_REQUEST',
      message: usage.message,
    })
    writeResponse(io, response)
    return STRONGFLOW_DELIVERY_CLI_EXIT_CODES.usage
  }
  let response: StrongFlowDeliveryResponse
  try {
    response = parseStrongFlowDeliveryResponseForRequest(
      requestValue,
      await invoker.invoke(
        requestValue,
        io.signal === undefined ? {} : { signal: io.signal },
      ),
    )
  } catch {
    response = materializeStrongFlowDeliveryFailure({
      requestId: requestValue.requestId,
      operation: requestValue.operation,
      code: io.signal?.aborted === true ? 'OPERATION_ABORTED' : 'INTERNAL_ERROR',
      message: io.signal?.aborted === true
        ? 'StrongFlow Delivery 请求已中止。'
        : 'StrongFlow Delivery 本地调用失败。',
    })
  }
  writeResponse(io, response)
  const interruptedBy = io.interruptedBy?.()
  if (!response.ok
    && response.error.code === 'OPERATION_ABORTED'
    && interruptedBy !== undefined) {
    return interruptedBy === 'SIGINT'
      ? STRONGFLOW_DELIVERY_CLI_EXIT_CODES.sigint
      : STRONGFLOW_DELIVERY_CLI_EXIT_CODES.sigterm
  }
  return responseExitCode(response)
}
