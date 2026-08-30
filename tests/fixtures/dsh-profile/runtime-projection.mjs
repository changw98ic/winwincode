const DEFAULT_ROW_LIMIT = 1_024;
const DEFAULT_DEDUPLICATION_LIMIT = 2_048;
const MAX_PROJECTION_LIMIT = 65_536;
export class DshProjectionError extends Error {
    code;
    constructor(code, message) {
        super(message);
        this.name = 'DshProjectionError';
        this.code = code;
    }
}
function projectionLimit(value, fallback, label) {
    const limit = value ?? fallback;
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > MAX_PROJECTION_LIMIT) {
        throw new Error(`${label} must be between 1 and ${MAX_PROJECTION_LIMIT}`);
    }
    return limit;
}
function isRecord(value) {
    return typeof value === 'object' && value !== null && !Array.isArray(value);
}
function nonEmptyString(value) {
    return typeof value === 'string' && value.length > 0 ? value : undefined;
}
function nestedRecord(record, key) {
    const value = record[key];
    return isRecord(value) ? value : undefined;
}
function eventFingerprint(event) {
    return JSON.stringify(event);
}
function terminalStatus(reason) {
    switch (reason) {
        case 'completed': return 'completed';
        case 'aborted': return 'aborted';
        case 'failed': return 'failed';
        case 'declined': return 'declined';
        case 'cancelled': return 'cancelled';
        case 'unknown': return 'completed';
        case undefined: return 'updated';
    }
}
function rowStatus(event) {
    if (event.kind === 'approval.requested' || event.kind === 'input.requested')
        return 'waiting';
    if (event.kind.endsWith('.started'))
        return 'active';
    if (event.kind === 'failure')
        return 'failed';
    if (event.kind.endsWith('.completed') || event.kind === 'turn.aborted') {
        return terminalStatus(event.terminalReason);
    }
    return 'updated';
}
function entityId(event) {
    const source = event.source;
    if (event.kind.startsWith('turn.'))
        return `turn:${source.turnId ?? event.id}`;
    if (event.kind.startsWith('tool.')) {
        return `tool:${source.toolCallId ?? source.itemId ?? event.id}`;
    }
    if (event.kind === 'approval.requested') {
        return `approval:${source.approvalId ?? event.id}`;
    }
    if (event.kind === 'input.requested') {
        return `input:${source.approvalId ?? source.toolCallId ?? event.id}`;
    }
    if (event.kind === 'plan.updated')
        return `plan:${source.itemId ?? source.turnId ?? event.id}`;
    if (event.kind.startsWith('subagent.')) {
        return `subagent:${source.agentThreadId ?? source.agentPath ?? source.toolCallId ?? event.id}`;
    }
    if (event.kind.startsWith('message.'))
        return `message:${source.itemId ?? event.id}`;
    if (event.kind.startsWith('reasoning.'))
        return `reasoning:${source.itemId ?? event.id}`;
    if (event.kind === 'diff.updated')
        return `diff:${source.turnId ?? source.sessionId}`;
    if (event.kind === 'usage.updated')
        return `usage:${source.turnId ?? source.sessionId}`;
    if (event.kind === 'session.configured')
        return `session:${source.sessionId}`;
    return `${event.kind}:${event.id}`;
}
function redundantItemType(event) {
    if (event.data.type !== 'item_started' && event.data.type !== 'item_completed')
        return undefined;
    const type = nonEmptyString(nestedRecord(event.data, 'item')?.type);
    return type === 'UserMessage'
        || type === 'AgentMessage'
        || type === 'Reasoning'
        || type === 'SubAgentActivity'
        || type === 'CollabAgentToolCall'
        ? type
        : undefined;
}
function legacyToolLifecycle(event) {
    return event.data.type === 'exec_command_begin'
        || event.data.type === 'exec_command_end'
        || event.data.type === 'mcp_tool_call_begin'
        || event.data.type === 'mcp_tool_call_end'
        || event.data.type === 'web_search_begin'
        || event.data.type === 'web_search_end'
        || event.data.type === 'image_generation_begin'
        || event.data.type === 'image_generation_end'
        || event.data.type === 'patch_apply_begin'
        || event.data.type === 'patch_apply_end'
        || event.data.type === 'dynamic_tool_call_request'
        || event.data.type === 'dynamic_tool_call_response';
}
function isTerminalRow(row) {
    return row.status === 'completed'
        || row.status === 'aborted'
        || row.status === 'failed'
        || row.status === 'declined'
        || row.status === 'cancelled';
}
function approvalKind(event) {
    switch (event.data.type) {
        case 'exec_approval_request': return 'exec';
        case 'apply_patch_approval_request': return 'patch';
        default: return 'interaction';
    }
}
function approvalOperationId(event) {
    return event.source.approvalId
        ?? nonEmptyString(event.data.call_id)
        ?? nonEmptyString(event.data.id)
        ?? event.id;
}
function outputText(data) {
    const item = nestedRecord(data, 'item');
    for (const candidate of [
        data.formatted_output,
        data.aggregated_output,
        data.stdout,
        data.result,
        data.message,
        item?.formatted_output,
        item?.aggregated_output,
        item?.stdout,
        item?.result,
        item?.error,
    ]) {
        if (typeof candidate === 'string' && candidate.length > 0)
            return candidate;
    }
    return JSON.stringify(data);
}
function itemName(data) {
    const item = nestedRecord(data, 'item');
    const invocation = nestedRecord(data, 'invocation');
    return nonEmptyString(item?.tool)
        ?? nonEmptyString(invocation?.tool)
        ?? nonEmptyString(item?.type)
        ?? nonEmptyString(data.type)
        ?? 'tool';
}
function itemArguments(data) {
    const item = nestedRecord(data, 'item');
    const invocation = nestedRecord(data, 'invocation');
    const value = item?.arguments ?? invocation?.arguments ?? item?.command ?? data.command ?? {};
    return typeof value === 'string' ? value : JSON.stringify(value);
}
function messageText(event) {
    const direct = nonEmptyString(event.data.message);
    if (direct !== undefined)
        return direct;
    const item = nestedRecord(event.data, 'item');
    const content = item?.content;
    if (!Array.isArray(content))
        return undefined;
    const text = content.flatMap((entry) => {
        if (!isRecord(entry))
            return [];
        return typeof entry.text === 'string' ? [entry.text] : [];
    }).join('');
    return text.length === 0 ? undefined : text;
}
function itemRole(event) {
    if (event.data.type === 'user_message')
        return 'user';
    if (event.data.type === 'agent_message')
        return 'assistant';
    const type = nonEmptyString(nestedRecord(event.data, 'item')?.type);
    if (type === 'UserMessage')
        return 'user';
    if (type === 'AgentMessage')
        return 'assistant';
    return undefined;
}
function turnReason(event) {
    switch (event.terminalReason) {
        case 'completed':
            return Object.freeze({ kind: 'completed' });
        case 'aborted':
            return Object.freeze({ kind: 'aborted', reason: Object.freeze({ kind: 'user' }) });
        case 'failed': {
            const error = nestedRecord(event.data, 'error');
            const message = nonEmptyString(error?.message)
                ?? nonEmptyString(event.data.message)
                ?? 'Codex turn failed';
            return Object.freeze({
                kind: 'error',
                error: Object.freeze({ code: 'CODEX_TURN_FAILED', message }),
            });
        }
        case 'cancelled':
            return Object.freeze({ kind: 'interrupted' });
        case 'declined':
        case 'unknown':
        case undefined:
            return Object.freeze({ kind: 'blocked' });
    }
    return Object.freeze({ kind: 'blocked' });
}
/** Rebuildable DSH chat/session and generic UI projection. */
export class DshRuntimeProjection {
    sessionId;
    roleId;
    rowLimit;
    deduplicationLimit;
    #provider;
    #model;
    #sequence = 0n;
    #status = 'idle';
    #turnCounter = 0;
    #activeTurnId;
    #turnNumbers = new Map();
    #rows = new Map();
    #pendingApprovals = new Map();
    #fingerprints = new Map();
    #pendingLegacyEntityIds = new Map();
    #latestDiff;
    #latestUsage;
    constructor(options) {
        try {
            if (options.sessionId.length === 0)
                throw new Error('sessionId must not be empty');
            if (options.roleId.length === 0)
                throw new Error('roleId must not be empty');
            this.sessionId = options.sessionId;
            this.roleId = options.roleId;
            this.#provider = options.provider ?? 'winwincode';
            this.#model = options.model ?? 'embedded-codex';
            this.rowLimit = projectionLimit(options.rowLimit, DEFAULT_ROW_LIMIT, 'rowLimit');
            this.deduplicationLimit = projectionLimit(options.deduplicationLimit, DEFAULT_DEDUPLICATION_LIMIT, 'deduplicationLimit');
        }
        catch (error) {
            throw new DshProjectionError('INVALID_PROJECTION_OPTIONS', error instanceof Error ? error.message : 'invalid projection options');
        }
    }
    get snapshot() {
        const snapshot = {
            schemaVersion: 1,
            sessionId: this.sessionId,
            roleId: this.roleId,
            asOfSequence: this.#sequence.toString(),
            status: this.#status,
            rows: Object.freeze([...this.#rows.values()].map(row => structuredClone(row))),
            pendingApprovals: Object.freeze([...this.#pendingApprovals.values()].map(approval => structuredClone(approval))),
            ...(this.#latestDiff === undefined ? {} : { latestDiff: structuredClone(this.#latestDiff) }),
            ...(this.#latestUsage === undefined
                ? {}
                : { latestUsage: structuredClone(this.#latestUsage) }),
        };
        return Object.freeze(snapshot);
    }
    pendingApproval(id) {
        const approval = this.#pendingApprovals.get(id);
        return approval === undefined ? undefined : structuredClone(approval);
    }
    apply(event) {
        this.#validateIdentity(event);
        const actual = BigInt(event.cursor.sequence);
        const expected = this.#sequence + 1n;
        const fingerprint = eventFingerprint(event);
        if (actual < expected) {
            const prior = this.#fingerprints.get(event.id);
            if (prior === fingerprint) {
                return Object.freeze({ changed: false, sessionAppends: Object.freeze([]) });
            }
            throw new DshProjectionError(prior === undefined ? 'RUNTIME_SEQUENCE_OUT_OF_ORDER' : 'RUNTIME_SEQUENCE_CONFLICT', prior === undefined
                ? `runtime event ${event.id} arrived behind cursor ${this.#sequence.toString()}`
                : `runtime event ${event.id} changed after projection`);
        }
        if (actual > expected) {
            throw new DshProjectionError('RUNTIME_SEQUENCE_MISSING', `runtime sequence ${expected.toString()} is missing before ${actual.toString()}`);
        }
        const redundantItem = redundantItemType(event);
        const rowId = redundantItem === undefined && !legacyToolLifecycle(event)
            ? this.#rowId(event)
            : undefined;
        this.#makeRoomFor(rowId);
        this.#sequence = actual;
        this.#fingerprints.set(event.id, fingerprint);
        while (this.#fingerprints.size > this.deduplicationLimit) {
            const oldest = this.#fingerprints.keys().next().value;
            if (oldest === undefined)
                break;
            this.#fingerprints.delete(oldest);
        }
        this.#updateRoute(event);
        this.#updateStatus(event);
        this.#updateApprovals(event);
        if (event.kind === 'diff.updated')
            this.#latestDiff = event.data;
        if (event.kind === 'usage.updated')
            this.#latestUsage = event.data;
        if (redundantItem !== undefined)
            this.#rememberLegacyEntity(event, redundantItem);
        if (rowId !== undefined)
            this.#upsertRow(event, rowId);
        this.#forgetLegacyEntity(event);
        const sessionAppends = Object.freeze(this.#sessionAppends(event));
        return Object.freeze({ changed: true, sessionAppends });
    }
    replay(events) {
        const appends = [];
        for (const event of events)
            appends.push(...this.apply(event).sessionAppends);
        return Object.freeze(appends);
    }
    #validateIdentity(event) {
        if (event.cursor.sessionId !== this.sessionId || event.source.sessionId !== this.sessionId) {
            throw new DshProjectionError('RUNTIME_SESSION_MISMATCH', `runtime event ${event.id} does not belong to session ${this.sessionId}`);
        }
        if (event.source.roleId !== this.roleId) {
            throw new DshProjectionError('RUNTIME_ROLE_MISMATCH', `runtime event ${event.id} does not belong to role ${this.roleId}`);
        }
    }
    #updateRoute(event) {
        if (event.kind !== 'session.configured')
            return;
        const settings = nestedRecord(event.data, 'thread_settings');
        const provider = nonEmptyString(event.data.model_provider_id)
            ?? nonEmptyString(settings?.model_provider_id);
        const model = nonEmptyString(event.data.model) ?? nonEmptyString(settings?.model);
        if (provider !== undefined)
            this.#provider = provider;
        if (model !== undefined)
            this.#model = model;
    }
    #updateStatus(event) {
        if (event.kind === 'turn.started') {
            this.#status = 'running';
            this.#activeTurnId = event.source.turnId ?? event.source.submissionId;
            this.#turnNumber(this.#activeTurnId);
            return;
        }
        if (event.kind === 'approval.requested' || event.kind === 'input.requested') {
            this.#status = 'awaiting_approval';
            return;
        }
        if (event.kind === 'failure') {
            this.#status = 'failed';
            return;
        }
        if (event.kind === 'turn.completed') {
            this.#status = event.terminalReason === 'failed' ? 'failed' : 'completed';
            this.#activeTurnId = undefined;
            return;
        }
        if (event.kind === 'turn.aborted') {
            this.#status = 'aborted';
            this.#activeTurnId = undefined;
        }
    }
    #updateApprovals(event) {
        if (event.kind === 'approval.requested' || event.kind === 'input.requested') {
            const id = event.source.approvalId ?? event.id;
            this.#pendingApprovals.set(id, Object.freeze({
                id,
                kind: approvalKind(event),
                operationId: approvalOperationId(event),
                source: event.source,
                payload: event.data,
            }));
            return;
        }
        if (event.kind.startsWith('tool.') && event.source.toolCallId !== undefined) {
            for (const [id, approval] of this.#pendingApprovals) {
                if (approval.operationId === event.source.toolCallId)
                    this.#pendingApprovals.delete(id);
            }
        }
        if (event.kind === 'turn.completed' || event.kind === 'turn.aborted') {
            for (const [id, approval] of this.#pendingApprovals) {
                if (approval.source.turnId === event.source.turnId)
                    this.#pendingApprovals.delete(id);
            }
        }
        if (this.#pendingApprovals.size === 0 && this.#status === 'awaiting_approval') {
            this.#status = 'running';
        }
    }
    #rowId(event) {
        const legacyKey = this.#legacyEntityKey(event);
        const pending = legacyKey === undefined ? undefined : this.#pendingLegacyEntityIds.get(legacyKey);
        if (pending !== undefined) {
            if (event.kind.startsWith('reasoning.'))
                return `reasoning:${pending}`;
            return `message:${pending}`;
        }
        return entityId(event);
    }
    #legacyEntityKey(event) {
        if (event.data.type === 'user_message')
            return 'UserMessage';
        if (event.data.type === 'agent_message')
            return 'AgentMessage';
        if (event.data.type === 'agent_reasoning' || event.data.type === 'agent_reasoning_raw_content') {
            return 'Reasoning';
        }
        return undefined;
    }
    #rememberLegacyEntity(event, type) {
        if (event.data.type !== 'item_completed')
            return;
        const itemId = event.source.itemId;
        if (itemId !== undefined)
            this.#pendingLegacyEntityIds.set(type, itemId);
    }
    #forgetLegacyEntity(event) {
        const key = this.#legacyEntityKey(event);
        if (key !== undefined)
            this.#pendingLegacyEntityIds.delete(key);
    }
    #upsertRow(event, id) {
        const existing = this.#rows.get(id);
        const row = Object.freeze({
            id,
            kind: event.kind,
            status: rowStatus(event),
            firstEventId: existing?.firstEventId ?? event.id,
            lastEventId: event.id,
            firstSequence: existing?.firstSequence ?? event.cursor.sequence,
            lastSequence: event.cursor.sequence,
            source: event.source,
            payload: event.data,
        });
        this.#rows.set(id, row);
    }
    #makeRoomFor(id) {
        if (id === undefined || this.#rows.has(id))
            return;
        if (this.#rows.size < this.rowLimit)
            return;
        for (const [id, row] of this.#rows) {
            if (isTerminalRow(row) && !id.startsWith('session:')) {
                this.#rows.delete(id);
                return;
            }
        }
        throw new DshProjectionError('PROJECTION_CAPACITY_EXCEEDED', `DSH runtime projection reached ${this.rowLimit} active rows`);
    }
    #turnNumber(turnId) {
        const existing = this.#turnNumbers.get(turnId);
        if (existing !== undefined)
            return existing;
        this.#turnCounter += 1;
        this.#turnNumbers.set(turnId, this.#turnCounter);
        return this.#turnCounter;
    }
    #eventTurn(event) {
        const turnId = event.source.turnId ?? this.#activeTurnId ?? event.source.submissionId;
        return this.#turnNumber(turnId);
    }
    #sessionAppends(event) {
        if (redundantItemType(event) !== undefined || legacyToolLifecycle(event))
            return [];
        const turn = this.#eventTurn(event);
        if (event.kind === 'turn.started') {
            return [
                {
                    sourceEventId: event.id,
                    type: 'turn/start',
                    data: Object.freeze({ turn }),
                },
                {
                    sourceEventId: event.id,
                    type: 'step/start',
                    data: Object.freeze({ turn, step: 1 }),
                },
            ];
        }
        if (event.kind === 'turn.completed' || event.kind === 'turn.aborted') {
            return [
                {
                    sourceEventId: event.id,
                    type: 'step/end',
                    data: Object.freeze({ turn, step: 1 }),
                },
                {
                    sourceEventId: event.id,
                    type: 'turn/end',
                    data: Object.freeze({
                        turn,
                        reason: turnReason(event),
                    }),
                },
            ];
        }
        if (event.kind === 'message.delta') {
            const delta = nonEmptyString(event.data.delta);
            if (delta === undefined)
                return [];
            return [{
                    sourceEventId: event.id,
                    type: 'assistant/chunk',
                    data: Object.freeze({
                        turn,
                        step: 1,
                        chunk: Object.freeze({ type: 'text-delta', index: 0, text: delta }),
                    }),
                }];
        }
        if (event.kind === 'reasoning.delta') {
            const delta = nonEmptyString(event.data.delta) ?? nonEmptyString(event.data.text);
            if (delta === undefined)
                return [];
            return [{
                    sourceEventId: event.id,
                    type: 'assistant/chunk',
                    data: Object.freeze({
                        turn,
                        step: 1,
                        chunk: Object.freeze({ type: 'reasoning-delta', index: 1, text: delta }),
                    }),
                }];
        }
        if (event.kind === 'message.completed') {
            const role = itemRole(event);
            const text = messageText(event);
            if (role === undefined || text === undefined)
                return [];
            if (role === 'user') {
                return [{
                        sourceEventId: event.id,
                        type: 'user/message',
                        data: Object.freeze({
                            id: `${event.id}:message`,
                            role: 'user',
                            content: Object.freeze([Object.freeze({ type: 'text', text })]),
                            source: Object.freeze({ kind: 'user' }),
                        }),
                        surface: Object.freeze({ surfaceOp: 'append' }),
                    }];
            }
            return [{
                    sourceEventId: event.id,
                    type: 'assistant/message',
                    data: Object.freeze({
                        turn,
                        step: 1,
                        message: Object.freeze({
                            id: `${event.id}:message`,
                            role: 'assistant',
                            content: Object.freeze([Object.freeze({ type: 'text', text })]),
                            source: Object.freeze({
                                kind: 'model',
                                provider: this.#provider,
                                model: this.#model,
                            }),
                        }),
                    }),
                    surface: Object.freeze({ surfaceOp: 'append' }),
                }];
        }
        if (event.kind === 'tool.started') {
            const callId = event.source.toolCallId;
            if (callId === undefined)
                return [];
            return [{
                    sourceEventId: event.id,
                    type: 'tool/call',
                    data: Object.freeze({
                        turn,
                        step: 1,
                        callId,
                        name: itemName(event.data),
                        arguments: itemArguments(event.data),
                    }),
                }];
        }
        if (event.kind === 'tool.completed') {
            const callId = event.source.toolCallId;
            if (callId === undefined)
                return [];
            const isError = event.terminalReason === 'failed'
                || event.terminalReason === 'declined'
                || event.terminalReason === 'cancelled';
            return [{
                    sourceEventId: event.id,
                    type: 'tool/result',
                    data: Object.freeze({
                        turn,
                        step: 1,
                        message: Object.freeze({
                            id: `${event.id}:message`,
                            role: 'user',
                            content: Object.freeze([Object.freeze({
                                    type: 'tool-result',
                                    toolCallId: callId,
                                    content: Object.freeze([Object.freeze({ type: 'text', text: outputText(event.data) })]),
                                    isError,
                                })]),
                            source: Object.freeze({ kind: 'tool', callId }),
                        }),
                    }),
                    surface: Object.freeze({ surfaceOp: 'append' }),
                }];
        }
        return [];
    }
}
/** One-shot bridge from a displayed approval to the exact suspended Codex operation. */
export class RuntimeApprovalRouter {
    #kernel;
    #projection;
    #submitted = new Set();
    #inFlight = new Set();
    constructor(kernel, projection) {
        this.#kernel = kernel;
        this.#projection = projection;
    }
    async resolve(resolution) {
        const pending = this.#projection.pendingApproval(resolution.approvalId);
        if (pending === undefined) {
            throw new DshProjectionError('APPROVAL_NOT_PENDING', `approval ${resolution.approvalId} is not pending`);
        }
        if (pending.kind === 'interaction') {
            throw new DshProjectionError('APPROVAL_KIND_UNSUPPORTED', `approval ${resolution.approvalId} is not an exec or patch approval`);
        }
        if (this.#submitted.has(pending.id) || this.#inFlight.has(pending.id)) {
            throw new DshProjectionError('APPROVAL_ALREADY_SUBMITTED', `approval ${resolution.approvalId} already has a submitted response`);
        }
        this.#inFlight.add(pending.id);
        try {
            const response = {
                sessionId: pending.source.kernelSessionId,
                kind: pending.kind,
                operationId: pending.operationId,
                decision: resolution.decision,
                ...(pending.source.turnId === undefined ? {} : { turnId: pending.source.turnId }),
            };
            const submissionId = await this.#kernel.resolveApproval(response);
            this.#submitted.add(pending.id);
            return submissionId;
        }
        finally {
            this.#inFlight.delete(pending.id);
        }
    }
}
