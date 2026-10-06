import { createInterface } from 'node:readline'

const input = createInterface({ input: process.stdin })
for await (const line of input) {
  const request = JSON.parse(line)
  if (request.id === undefined) continue
  let result
  switch (request.method) {
    case 'initialize':
      result = {
        protocolVersion: request.params.protocolVersion,
        capabilities: { tools: {} },
        serverInfo: { name: 'code-mode-fixture', version: '1' },
      }
      break
    case 'tools/list':
      result = { tools: [{
        name: 'echo',
        description: 'Echo a Code Mode fixture value',
        annotations: { readOnlyHint: true, destructiveHint: false, openWorldHint: false },
        inputSchema: {
          type: 'object', properties: { value: { type: 'string' } },
          required: ['value'], additionalProperties: false,
        },
      }] }
      break
    case 'tools/call':
      if (request.params.name !== 'echo') throw new Error('unknown fixture tool')
      result = { content: [{ type: 'text', text: request.params.arguments.value }] }
      break
    case 'ping':
      result = {}
      break
    default:
      process.stdout.write(`${JSON.stringify({
        jsonrpc: '2.0', id: request.id,
        error: { code: -32601, message: 'Method not found' },
      })}\n`)
      continue
  }
  process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', id: request.id, result })}\n`)
}
