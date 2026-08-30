import { createHash } from 'node:crypto';
class ModelPortError extends Error {
    failure;
    constructor(failure, cause) {
        super(failure.message, cause === undefined ? undefined : { cause });
        this.name = 'ModelPortError';
        this.failure = Object.freeze({ ...failure });
    }
}
const DSH_CALL_CONFIG_KEYS = new Set([
    'provider',
    'model',
    'reasoningEffort',
    'temperature',
    'maxTokens',
    'stop',
]);
const SAFE_FAILURE_CODE = /^[A-Z][A-Z0-9_]{0,63}$/u;
const CUSTOM_TOOL_PARAMETERS = Object.freeze({
    type: 'object',
    properties: Object.freeze({
        input: Object.freeze({ type: 'string' }),
    }),
    required: Object.freeze(['input']),
    additionalProperties: false,
});
function isRecord(value) {
    return typeof value === 'object' && value !== null && !Array.isArray(value);
}
function preparedCallConfig(value) {
    const maxTokens = isRecord(value) ? value.maxTokens : undefined;
    if (!isRecord(value)
        || Object.keys(value).some(key => !DSH_CALL_CONFIG_KEYS.has(key))
        || typeof value.provider !== 'string'
        || value.provider.length === 0
        || typeof value.model !== 'string'
        || value.model.length === 0
        || (value.reasoningEffort !== undefined && typeof value.reasoningEffort !== 'string')
        || (value.temperature !== undefined && (typeof value.temperature !== 'number' || !Number.isFinite(value.temperature)))
        || (maxTokens !== undefined && (typeof maxTokens !== 'number' || !Number.isSafeInteger(maxTokens) || maxTokens <= 0))
        || (value.stop !== undefined && (!Array.isArray(value.stop)
            || value.stop.some(entry => typeof entry !== 'string'))))
        return bridgeError('MODEL_PORT_PREPARED_CONFIG_INVALID', 'DSH prepared a model call with an invalid or credential-bearing configuration');
    const stop = value.stop;
    return Object.freeze({
        provider: value.provider,
        model: value.model,
        ...(value.reasoningEffort === undefined
            ? {}
            : { reasoningEffort: value.reasoningEffort }),
        ...(value.temperature === undefined ? {} : { temperature: value.temperature }),
        ...(maxTokens === undefined ? {} : { maxTokens }),
        ...(stop === undefined ? {} : { stop: [...stop] }),
    });
}
function bridgeError(code, message) {
    throw new ModelPortError({ code, message });
}
function requiredString(record, key, context) {
    const value = record[key];
    if (typeof value !== 'string' || value.length === 0) {
        return bridgeError('MODEL_PORT_REQUEST_INVALID', `${context}.${key} must be a non-empty string`);
    }
    return value;
}
function optionalString(record, key) {
    const value = record[key];
    return typeof value === 'string' && value.length > 0 ? value : undefined;
}
function parseResponsesRequest(request) {
    if (!isRecord(request.request)) {
        return bridgeError('MODEL_PORT_REQUEST_INVALID', 'request payload must be an object');
    }
    const value = request.request;
    const model = requiredString(value, 'model', 'request');
    const input = value.input;
    if (!Array.isArray(input)) {
        return bridgeError('MODEL_PORT_REQUEST_INVALID', 'request.input must be an array');
    }
    const instructions = value.instructions;
    if (instructions !== undefined && typeof instructions !== 'string') {
        return bridgeError('MODEL_PORT_REQUEST_INVALID', 'request.instructions must be a string');
    }
    const tools = value.tools;
    if (tools !== undefined && tools !== null && !Array.isArray(tools)) {
        return bridgeError('MODEL_PORT_REQUEST_INVALID', 'request.tools must be an array');
    }
    const toolChoice = value.tool_choice;
    if (typeof toolChoice !== 'string') {
        return bridgeError('MODEL_PORT_REQUEST_INVALID', 'request.tool_choice must be a string');
    }
    return {
        model,
        instructions: instructions ?? '',
        input,
        tool_choice: toolChoice,
        ...(tools === undefined ? {} : { tools }),
        ...(value.reasoning === undefined ? {} : { reasoning: value.reasoning }),
        ...(value.text === undefined ? {} : { text: value.text }),
        ...(value.service_tier === undefined ? {} : { service_tier: value.service_tier }),
    };
}
function normalizedToolName(value) {
    const normalized = value.replace(/[^A-Za-z0-9_-]/gu, '_');
    return (normalized.length === 0 ? 'tool' : normalized).slice(0, 56);
}
function uniqueToolName(candidate, used) {
    const base = normalizedToolName(candidate);
    let value = base;
    let suffix = 2;
    while (used.has(value)) {
        value = `${base.slice(0, 52)}_${suffix}`;
        suffix += 1;
    }
    used.add(value);
    return value;
}
function translateTools(rawTools) {
    const schemas = [];
    const bindings = new Map();
    const used = new Set();
    const addTool = (raw, namespace, namespaceDescription) => {
        if (!isRecord(raw)) {
            return bridgeError('MODEL_PORT_REQUEST_INVALID', 'tool definition must be an object');
        }
        const type = requiredString(raw, 'type', 'tool');
        if (type !== 'function' && type !== 'custom') {
            return bridgeError('UNSUPPORTED_TOOL', `Codex tool type "${type}" is not supported by DSH`);
        }
        const name = requiredString(raw, 'name', 'tool');
        const description = typeof raw.description === 'string' ? raw.description : '';
        const exposedName = uniqueToolName(namespace === undefined ? name : `${namespace}__${name}`, used);
        let parameters;
        if (type === 'function') {
            if (!isRecord(raw.parameters)) {
                return bridgeError('MODEL_PORT_REQUEST_INVALID', `function tool "${name}" has no JSON-schema parameters`);
            }
            if (raw.output_schema !== undefined && raw.output_schema !== null) {
                return bridgeError('UNSUPPORTED_TOOL', `function tool "${name}" requires an output schema that DSH cannot enforce`);
            }
            parameters = raw.parameters;
        }
        else {
            parameters = CUSTOM_TOOL_PARAMETERS;
        }
        const qualifiedDescription = namespaceDescription === undefined || namespaceDescription === ''
            ? description
            : `${namespaceDescription}\n\n${description}`;
        schemas.push({ name: exposedName, description: qualifiedDescription, parameters });
        bindings.set(exposedName, {
            exposedName,
            kind: type,
            name,
            ...(namespace === undefined ? {} : { namespace }),
        });
    };
    for (const raw of rawTools ?? []) {
        if (!isRecord(raw)) {
            return bridgeError('MODEL_PORT_REQUEST_INVALID', 'tool definition must be an object');
        }
        if (raw.type === 'namespace') {
            const namespace = requiredString(raw, 'name', 'namespace tool');
            const children = raw.tools;
            if (!Array.isArray(children)) {
                return bridgeError('MODEL_PORT_REQUEST_INVALID', `namespace "${namespace}" has no tools`);
            }
            const description = typeof raw.description === 'string' ? raw.description : '';
            for (const child of children)
                addTool(child, namespace, description);
            continue;
        }
        addTool(raw);
    }
    return {
        ...(schemas.length === 0 ? {} : { schemas }),
        bindings,
    };
}
function textContent(content, context) {
    if (!Array.isArray(content)) {
        return bridgeError('MODEL_PORT_REQUEST_INVALID', `${context}.content must be an array`);
    }
    const parts = [];
    for (const raw of content) {
        if (!isRecord(raw)) {
            return bridgeError('MODEL_PORT_REQUEST_INVALID', `${context} content must be an object`);
        }
        if (raw.type === 'input_text' || raw.type === 'output_text') {
            if (typeof raw.text !== 'string') {
                return bridgeError('MODEL_PORT_REQUEST_INVALID', `${context} text must be a string`);
            }
            parts.push(raw.text);
            continue;
        }
        return bridgeError('UNSUPPORTED_CONTENT', `${context} content type "${String(raw.type)}" is not supported by the DSH model bridge`);
    }
    return parts.join('\n');
}
function toolResultContent(output, context) {
    if (typeof output === 'string')
        return [{ type: 'text', text: output }];
    if (isRecord(output) && typeof output.content === 'string') {
        return [{ type: 'text', text: output.content }];
    }
    if (!Array.isArray(output)) {
        return bridgeError('UNSUPPORTED_CONTENT', `${context} output is not textual`);
    }
    return [{ type: 'text', text: textContent(output, context) }];
}
function bindingForHistory(bindings, kind, name, namespace) {
    for (const binding of bindings.values()) {
        if (binding.kind === kind && binding.name === name && binding.namespace === namespace) {
            return binding;
        }
    }
    return {
        exposedName: normalizedToolName(namespace === undefined ? name : `${namespace}__${name}`),
        kind,
        name,
        ...(namespace === undefined ? {} : { namespace }),
    };
}
function message(id, role, content, source) {
    return { id, role, content, source };
}
function translateInput(input, request, model, bindings) {
    const messages = [];
    const systemFragments = [];
    let pendingAssistant = null;
    const flushAssistant = () => {
        if (pendingAssistant === null)
            return;
        if (pendingAssistant.content.length > 0) {
            messages.push(message(pendingAssistant.id, 'assistant', pendingAssistant.content, { kind: 'model', provider: request.provider, model }));
        }
        pendingAssistant = null;
    };
    const appendAssistant = (id, blocks) => {
        pendingAssistant ??= { id, content: [] };
        pendingAssistant.content.push(...blocks);
    };
    let sequence = 0;
    for (const raw of input) {
        sequence += 1;
        if (!isRecord(raw)) {
            return bridgeError('MODEL_PORT_REQUEST_INVALID', 'request input item must be an object');
        }
        const type = requiredString(raw, 'type', 'request input item');
        const id = optionalString(raw, 'id') ?? `codex-${request.requestId}-${sequence}`;
        switch (type) {
            case 'message': {
                const role = requiredString(raw, 'role', 'message');
                const text = textContent(raw.content, 'message');
                if (role === 'developer' || role === 'system') {
                    flushAssistant();
                    if (text !== '')
                        systemFragments.push(text);
                }
                else if (role === 'user') {
                    flushAssistant();
                    messages.push(message(id, 'user', [{ type: 'text', text }], { kind: 'user' }));
                }
                else if (role === 'assistant') {
                    if (text !== '')
                        appendAssistant(id, [{ type: 'text', text }]);
                }
                else {
                    return bridgeError('UNSUPPORTED_CONTENT', `message role "${role}" is not supported`);
                }
                break;
            }
            case 'reasoning': {
                const fragments = [];
                for (const field of ['summary', 'content']) {
                    const values = raw[field];
                    if (values === undefined || values === null)
                        continue;
                    if (!Array.isArray(values)) {
                        return bridgeError('MODEL_PORT_REQUEST_INVALID', `reasoning.${field} must be an array`);
                    }
                    for (const value of values) {
                        if (!isRecord(value) || typeof value.text !== 'string') {
                            return bridgeError('MODEL_PORT_REQUEST_INVALID', `reasoning.${field} is invalid`);
                        }
                        fragments.push(value.text);
                    }
                }
                if (fragments.length > 0) {
                    appendAssistant(id, [{ type: 'reasoning', text: fragments.join('\n') }]);
                }
                break;
            }
            case 'function_call': {
                const name = requiredString(raw, 'name', 'function call');
                const namespace = optionalString(raw, 'namespace');
                const binding = bindingForHistory(bindings, 'function', name, namespace);
                appendAssistant(id, [{
                        type: 'tool-call',
                        id: requiredString(raw, 'call_id', 'function call'),
                        name: binding.exposedName,
                        arguments: requiredString(raw, 'arguments', 'function call'),
                    }]);
                break;
            }
            case 'custom_tool_call': {
                const name = requiredString(raw, 'name', 'custom tool call');
                const namespace = optionalString(raw, 'namespace');
                const binding = bindingForHistory(bindings, 'custom', name, namespace);
                appendAssistant(id, [{
                        type: 'tool-call',
                        id: requiredString(raw, 'call_id', 'custom tool call'),
                        name: binding.exposedName,
                        arguments: JSON.stringify({ input: requiredString(raw, 'input', 'custom tool call') }),
                    }]);
                break;
            }
            case 'function_call_output':
            case 'custom_tool_call_output': {
                flushAssistant();
                const callId = requiredString(raw, 'call_id', type);
                messages.push(message(id, 'user', [{
                        type: 'tool-result',
                        toolCallId: callId,
                        content: toolResultContent(raw.output, type),
                        isError: false,
                    }], { kind: 'tool', callId }));
                break;
            }
            default:
                return bridgeError('UNSUPPORTED_CONTENT', `Codex input item type "${type}" is not supported by the DSH model bridge`);
        }
    }
    flushAssistant();
    return { messages, systemFragments };
}
function reasoningEffort(raw) {
    if (raw === undefined || raw === null)
        return undefined;
    if (!isRecord(raw)) {
        return bridgeError('MODEL_PORT_REQUEST_INVALID', 'request.reasoning must be an object');
    }
    if (raw.effort !== undefined && typeof raw.effort !== 'string') {
        return bridgeError('MODEL_PORT_REQUEST_INVALID', 'request.reasoning.effort must be a string');
    }
    return raw.effort;
}
function translateRequest(request) {
    const responses = parseResponsesRequest(request);
    if (responses.tool_choice !== 'auto') {
        return bridgeError('UNSUPPORTED_OPTION', `tool choice "${responses.tool_choice}" is not supported by DSH`);
    }
    if (isRecord(responses.text)) {
        if (responses.text.format !== undefined && responses.text.format !== null) {
            return bridgeError('UNSUPPORTED_OPTION', 'structured output schemas are not supported by DSH');
        }
        if (responses.text.verbosity !== undefined && responses.text.verbosity !== null) {
            return bridgeError('UNSUPPORTED_OPTION', 'model verbosity is not supported by DSH');
        }
    }
    else if (responses.text !== undefined && responses.text !== null) {
        return bridgeError('MODEL_PORT_REQUEST_INVALID', 'request.text must be an object');
    }
    if (responses.service_tier !== undefined && responses.service_tier !== null) {
        return bridgeError('UNSUPPORTED_OPTION', 'service tiers are not supported by DSH');
    }
    const translatedTools = translateTools(responses.tools);
    const translatedInput = translateInput(responses.input, request, responses.model, translatedTools.bindings);
    const systemFragments = [responses.instructions, ...translatedInput.systemFragments]
        .filter(fragment => fragment !== '');
    const effort = reasoningEffort(responses.reasoning);
    return {
        config: {
            provider: request.provider,
            model: responses.model,
            ...(effort === undefined ? {} : { reasoningEffort: effort }),
        },
        messages: translatedInput.messages,
        ...(systemFragments.length === 0 ? {} : { system: systemFragments.join('\n\n') }),
        ...(translatedTools.schemas === undefined ? {} : { tools: translatedTools.schemas }),
        toolBindings: translatedTools.bindings,
    };
}
function failureFromUnknown(error) {
    if (error instanceof ModelPortError)
        return error.failure;
    if (isRecord(error) && isRecord(error.failure)) {
        const failure = error.failure;
        if (typeof failure.code === 'string'
            && SAFE_FAILURE_CODE.test(failure.code)
            && typeof failure.message === 'string') {
            const status = typeof failure.status === 'number'
                && Number.isInteger(failure.status)
                && failure.status >= 100
                && failure.status <= 599
                ? failure.status
                : undefined;
            const retryAfter = typeof failure.providerRetryAfterMs === 'number'
                && Number.isFinite(failure.providerRetryAfterMs)
                && failure.providerRetryAfterMs > 0
                && failure.providerRetryAfterMs <= Number.MAX_SAFE_INTEGER
                ? failure.providerRetryAfterMs
                : undefined;
            const providerRequestId = typeof failure.requestId === 'string'
                && failure.requestId.length > 0
                ? `sha256:${createHash('sha256').update(failure.requestId).digest('hex')}`
                : undefined;
            return {
                code: failure.code,
                message: `DSH model request failed with code ${failure.code}`,
                ...(status === undefined ? {} : { status }),
                ...(retryAfter === undefined ? {} : { providerRetryAfterMillis: retryAfter }),
                ...(providerRequestId === undefined ? {} : { providerRequestId }),
            };
        }
    }
    return {
        code: 'MODEL_RUNTIME_FAILED',
        message: 'DSH model runtime failed without a structured failure',
    };
}
function tokenUsage(usage) {
    const cacheRead = usage.cacheReadTokens ?? 0;
    const cacheWrite = usage.cacheWriteTokens ?? 0;
    const input = usage.inputTokens + cacheRead + cacheWrite;
    return {
        input_tokens: input,
        cached_input_tokens: cacheRead,
        cache_write_input_tokens: cacheWrite,
        output_tokens: usage.outputTokens,
        reasoning_output_tokens: usage.reasoningTokens ?? 0,
        total_tokens: input + usage.outputTokens,
    };
}
function itemId(prefix, requestId, index) {
    return `${prefix}_${requestId.replace(/[^A-Za-z0-9-]/gu, '-')}-${index}`;
}
function outputItem(block, bindings) {
    switch (block.type) {
        case 'text':
            return {
                type: 'message',
                id: block.itemId,
                role: 'assistant',
                content: [{ type: 'output_text', text: block.text }],
                phase: 'final_answer',
            };
        case 'reasoning':
            return {
                type: 'reasoning',
                id: block.itemId,
                summary: [{ type: 'summary_text', text: block.text }],
                encrypted_content: null,
            };
        case 'tool-call': {
            const exposedName = block.exposedName;
            if (exposedName === undefined) {
                return bridgeError('INVALID_TOOL_CALL', 'DSH tool call ended without a tool name');
            }
            const binding = bindings.get(exposedName);
            if (binding === undefined) {
                return bridgeError('INVALID_TOOL_CALL', `DSH requested unknown tool "${exposedName}"`);
            }
            const callId = block.callId;
            if (callId === undefined || callId.length === 0) {
                return bridgeError('INVALID_TOOL_CALL', 'DSH tool call ended without a call ID');
            }
            if (binding.kind === 'function') {
                return {
                    type: 'function_call',
                    id: block.itemId,
                    name: binding.name,
                    ...(binding.namespace === undefined ? {} : { namespace: binding.namespace }),
                    arguments: block.text,
                    call_id: callId,
                };
            }
            if (block.text === '') {
                return {
                    type: 'custom_tool_call',
                    id: block.itemId,
                    call_id: callId,
                    name: binding.name,
                    ...(binding.namespace === undefined ? {} : { namespace: binding.namespace }),
                    input: '',
                };
            }
            let input;
            try {
                input = JSON.parse(block.text);
            }
            catch (error) {
                throw new ModelPortError({
                    code: 'INVALID_TOOL_CALL',
                    message: `DSH custom tool "${exposedName}" returned invalid JSON arguments`,
                }, error);
            }
            if (!isRecord(input) || typeof input.input !== 'string') {
                return bridgeError('INVALID_TOOL_CALL', `DSH custom tool "${exposedName}" did not return a string input`);
            }
            return {
                type: 'custom_tool_call',
                id: block.itemId,
                call_id: callId,
                name: binding.name,
                ...(binding.namespace === undefined ? {} : { namespace: binding.namespace }),
                input: input.input,
            };
        }
    }
}
function startBlock(chunk, requestId) {
    switch (chunk.blockType) {
        case 'text':
            return {
                index: chunk.index,
                type: 'text',
                itemId: itemId('msg', requestId, chunk.index),
                text: '',
                started: true,
                finalized: false,
            };
        case 'reasoning':
            return {
                index: chunk.index,
                type: 'reasoning',
                itemId: itemId('rs', requestId, chunk.index),
                text: '',
                started: true,
                finalized: false,
            };
        case 'tool-call':
            return {
                index: chunk.index,
                type: 'tool-call',
                itemId: itemId('fc', requestId, chunk.index),
                text: '',
                started: false,
                finalized: false,
            };
        default:
            return bridgeError('UNSUPPORTED_CONTENT', `DSH stream block type "${chunk.blockType}" is not supported by Codex`);
    }
}
/** Direct, credential-free adapter from the embedded Codex kernel to `ctx.llm`. */
export class DshModelPort {
    #llm;
    constructor(llm) {
        this.#llm = llm;
    }
    async *stream(request, signal) {
        try {
            const translated = translateRequest(request);
            const prepared = await this.#llm.prepareCall(translated.config, signal);
            const config = preparedCallConfig(prepared.config);
            const options = {
                ...config,
                messages: translated.messages,
                ...(translated.system === undefined ? {} : { system: translated.system }),
                ...(translated.tools === undefined ? {} : { tools: translated.tools }),
                signal,
                sessionId: request.sessionId,
            };
            const chunks = prepared.stream(options);
            const blocks = new Map();
            let activeIndex;
            let usage;
            let terminal = false;
            yield { type: 'created' };
            yield { type: 'server_model', model: config.model };
            for await (const chunk of chunks) {
                if (terminal) {
                    return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH emitted data after finish');
                }
                switch (chunk.type) {
                    case 'block-start': {
                        if (blocks.has(chunk.index)) {
                            return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH reopened a stream block');
                        }
                        if (activeIndex !== undefined) {
                            const active = blocks.get(activeIndex);
                            if (active === undefined || active.type === 'tool-call') {
                                return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH active block is invalid');
                            }
                            if (active.type === 'reasoning') {
                                yield {
                                    type: 'reasoning_summary_done',
                                    itemId: active.itemId,
                                    text: active.text,
                                    summaryIndex: 0,
                                };
                            }
                            yield {
                                type: 'output_item_done',
                                item: outputItem(active, translated.toolBindings),
                            };
                            active.finalized = true;
                            activeIndex = undefined;
                        }
                        const block = startBlock(chunk, request.requestId);
                        blocks.set(chunk.index, block);
                        if (block.type === 'text') {
                            activeIndex = chunk.index;
                            yield { type: 'output_item_added', item: outputItem(block, translated.toolBindings) };
                        }
                        else if (block.type === 'reasoning') {
                            activeIndex = chunk.index;
                            yield { type: 'output_item_added', item: outputItem(block, translated.toolBindings) };
                            yield { type: 'reasoning_summary_part_added', summaryIndex: 0 };
                        }
                        break;
                    }
                    case 'text-delta': {
                        const block = blocks.get(chunk.index);
                        if (block?.type !== 'text') {
                            return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH text delta has no text block');
                        }
                        if (block.finalized) {
                            return bridgeError('UNSUPPORTED_STREAM_SHAPE', 'DSH resumed a text block after another output block started');
                        }
                        block.text += chunk.text;
                        yield { type: 'output_text_delta', delta: chunk.text };
                        break;
                    }
                    case 'reasoning-delta': {
                        const block = blocks.get(chunk.index);
                        if (block?.type !== 'reasoning') {
                            return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH reasoning delta has no reasoning block');
                        }
                        if (block.finalized) {
                            return bridgeError('UNSUPPORTED_STREAM_SHAPE', 'DSH resumed a reasoning block after another output block started');
                        }
                        block.text += chunk.text;
                        yield { type: 'reasoning_summary_delta', delta: chunk.text, summaryIndex: 0 };
                        break;
                    }
                    case 'tool-call-delta': {
                        const block = blocks.get(chunk.index);
                        if (block?.type !== 'tool-call') {
                            return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH tool delta has no tool block');
                        }
                        block.callId ??= chunk.id;
                        if (block.callId !== chunk.id) {
                            return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH changed a tool call ID');
                        }
                        if (chunk.name !== undefined)
                            block.exposedName ??= chunk.name;
                        block.text += chunk.argumentsDelta;
                        break;
                    }
                    case 'block-end': {
                        const block = blocks.get(chunk.index);
                        if (block === undefined) {
                            return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH ended an unknown stream block');
                        }
                        if (chunk.block.type !== block.type) {
                            return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH changed a stream block type');
                        }
                        if (chunk.block.type === 'text' || chunk.block.type === 'reasoning') {
                            if (block.finalized) {
                                if (block.text !== chunk.block.text) {
                                    return bridgeError('UNSUPPORTED_STREAM_SHAPE', 'DSH changed a finalized block after another output block started');
                                }
                                blocks.delete(chunk.index);
                                break;
                            }
                            if (activeIndex !== chunk.index) {
                                return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH ended an inactive stream block');
                            }
                            block.text = chunk.block.text;
                        }
                        else if (chunk.block.type === 'tool-call') {
                            block.callId = chunk.block.id;
                            block.exposedName = chunk.block.name;
                            block.text = chunk.block.arguments;
                            block.started = true;
                            yield {
                                type: 'output_item_added',
                                item: outputItem({ ...block, text: '' }, translated.toolBindings),
                            };
                        }
                        else {
                            return bridgeError('UNSUPPORTED_CONTENT', 'DSH returned an unsupported output block');
                        }
                        if (block.type === 'reasoning') {
                            yield {
                                type: 'reasoning_summary_done',
                                itemId: block.itemId,
                                text: block.text,
                                summaryIndex: 0,
                            };
                        }
                        yield { type: 'output_item_done', item: outputItem(block, translated.toolBindings) };
                        blocks.delete(chunk.index);
                        if (activeIndex === chunk.index)
                            activeIndex = undefined;
                        break;
                    }
                    case 'usage':
                        usage = tokenUsage(chunk.usage);
                        break;
                    case 'finish': {
                        if (blocks.size !== 0) {
                            return bridgeError('MODEL_PORT_PROTOCOL_INVALID', 'DSH finished with an open block');
                        }
                        terminal = true;
                        switch (chunk.reason.kind) {
                            case 'stop':
                                yield {
                                    type: 'completed',
                                    responseId: request.requestId,
                                    ...(usage === undefined ? {} : { tokenUsage: usage }),
                                    endTurn: true,
                                };
                                break;
                            case 'tool-calls':
                                yield {
                                    type: 'completed',
                                    responseId: request.requestId,
                                    ...(usage === undefined ? {} : { tokenUsage: usage }),
                                    endTurn: false,
                                };
                                break;
                            case 'max-tokens':
                                yield {
                                    type: 'error',
                                    error: {
                                        code: 'MAX_TOKENS',
                                        message: 'DSH model response reached its output-token limit',
                                    },
                                };
                                break;
                            case 'aborted':
                            case 'error':
                                yield { type: 'error', error: failureFromUnknown({ failure: chunk.reason.failure }) };
                                break;
                        }
                        break;
                    }
                }
            }
            if (!terminal) {
                yield {
                    type: 'error',
                    error: {
                        code: 'STREAM_CLOSED',
                        message: 'DSH model stream ended without a finish chunk',
                    },
                };
            }
        }
        catch (error) {
            yield { type: 'error', error: failureFromUnknown(error) };
        }
    }
}
