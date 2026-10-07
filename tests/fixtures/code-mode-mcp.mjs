import { createInterface } from 'node:readline'
import { readFile } from 'node:fs/promises'

let smokeExecutions = 0

const input = createInterface({ input: process.stdin })
for await (const line of input) {
  const request = JSON.parse(line)
  if (request.id === undefined) continue
  let result
  switch (request.method) {
    case 'initialize':
      result = {
        protocolVersion: request.params.protocolVersion,
        capabilities: { tools: {}, resources: {} },
        serverInfo: { name: 'code-mode-fixture', version: '1' },
      }
      break
    case 'tools/list':
      result = { tools: [{
        name: 'public_smoke', description: 'Read the native fixture source snapshot',
        annotations: { readOnlyHint: true, destructiveHint: false, openWorldHint: false },
        inputSchema: { type: 'object', properties: {}, additionalProperties: false },
      }, {
        name: 'echo',
        description: 'Echo a Code Mode fixture value',
        annotations: { readOnlyHint: true, destructiveHint: false, openWorldHint: false },
        inputSchema: {
          type: 'object', properties: { value: { type: 'string' } },
          required: ['value'], additionalProperties: false,
        },
      }, {
        name: 'media',
        description: 'Return an image and audio through the native tool protocol',
        annotations: { readOnlyHint: true, destructiveHint: false, openWorldHint: false },
        inputSchema: { type: 'object', properties: {}, additionalProperties: false },
      }] }
      break
    case 'tools/call':
      if (request.params.name === 'public_smoke') {
        await new Promise(resolve => setTimeout(resolve, 150))
        const report = { source: await readFile('source.txt', 'utf8'), execution: ++smokeExecutions }
        result = { structuredContent: report, content: [{ type: 'text', text: JSON.stringify(report) }] }
        break
      }
      if (request.params.name === 'media') {
        result = { content: [
          { type: 'image', mimeType: 'image/png', data: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==' },
          { type: 'audio', mimeType: 'audio/wav', data: 'UklGRjQAAABXQVZFZm10IBAAAAABAAEAQB8AAIA+AAACABAAZGF0YRAAAAAAAAAAAAAAAAAAAAAAAAAA' },
        ] }
        break
      }
      if (request.params.name !== 'echo') throw new Error('unknown fixture tool')
      result = { content: [{ type: 'text', text: request.params.arguments.value }] }
      break
    case 'resources/list':
      result = { resources: [{ uri: 'fixture://readme', name: 'readme', mimeType: 'text/plain' }] }
      break
    case 'resources/templates/list':
      result = { resourceTemplates: [{ uriTemplate: 'fixture://{name}', name: 'fixture-template' }] }
      break
    case 'resources/read':
      result = { contents: [{ uri: request.params.uri, mimeType: 'text/plain', text: 'resource-through-core' }] }
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
