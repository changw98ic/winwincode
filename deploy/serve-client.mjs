#!/usr/bin/env node

import { createServer } from 'node:http'
import { readFile, stat } from 'node:fs/promises'
import { extname, resolve, sep } from 'node:path'

const root = resolve(process.env.WWC_CLIENT_DIST ?? '/app/public')
const host = process.env.HOST ?? '0.0.0.0'
const port = Number.parseInt(process.env.PORT ?? '8080', 10)
const serverUrl = process.env.WWC_SERVER_URL ?? ''
const csp = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src https: wss:; object-src 'none'; base-uri 'none'; form-action 'none'"
const contentTypes = new Map([
  ['.css', 'text/css; charset=utf-8'],
  ['.html', 'text/html; charset=utf-8'],
  ['.js', 'text/javascript; charset=utf-8'],
  ['.json', 'application/json; charset=utf-8'],
  ['.svg', 'image/svg+xml'],
  ['.woff2', 'font/woff2'],
])

if (!Number.isSafeInteger(port) || port < 1 || port > 65_535) {
  throw new Error('PORT must be an integer from 1 to 65535')
}
if (serverUrl !== '') {
  const url = new URL(serverUrl)
  if (url.protocol !== 'https:' || url.username || url.password || url.search || url.hash) {
    throw new Error('WWC_SERVER_URL must be an HTTPS origin without credentials, query, or fragment')
  }
}

function headers(contentType, cacheControl = 'no-cache') {
  return {
    'Cache-Control': cacheControl,
    'Content-Security-Policy': csp,
    'Content-Type': contentType,
    'Cross-Origin-Opener-Policy': 'same-origin',
    'Referrer-Policy': 'no-referrer',
    'X-Content-Type-Options': 'nosniff',
    'X-Frame-Options': 'DENY',
  }
}

async function staticFile(pathname) {
  let decoded
  try {
    decoded = decodeURIComponent(pathname)
  } catch {
    return null
  }
  const candidate = resolve(root, `.${decoded}`)
  if (candidate !== root && !candidate.startsWith(`${root}${sep}`)) return null
  try {
    return (await stat(candidate)).isFile() ? candidate : null
  } catch {
    return null
  }
}

const server = createServer(async (request, response) => {
  if (request.method !== 'GET' && request.method !== 'HEAD') {
    response.writeHead(405, { Allow: 'GET, HEAD' })
    response.end()
    return
  }
  const pathname = new URL(request.url ?? '/', 'http://localhost').pathname
  if (pathname === '/health') {
    response.writeHead(200, headers('application/json; charset=utf-8', 'no-store'))
    response.end(request.method === 'HEAD' ? undefined : '{"status":"ok"}\n')
    return
  }
  if (pathname === '/runtime-config.js') {
    const body = `// SPDX-License-Identifier: Apache-2.0\nglobalThis.__WINWINCODE_CLIENT_CONFIG__ = Object.freeze({ serverUrl: ${JSON.stringify(serverUrl)} })\n`
    response.writeHead(200, headers('text/javascript; charset=utf-8', 'no-store'))
    response.end(request.method === 'HEAD' ? undefined : body)
    return
  }

  const path = await staticFile(pathname === '/' ? '/index.html' : pathname)
  if (path === null) {
    response.writeHead(404, headers('text/plain; charset=utf-8'))
    response.end(request.method === 'HEAD' ? undefined : 'Not found\n')
    return
  }
  const bytes = await readFile(path)
  const cacheControl = /\.[A-Z0-9_-]{8,}\./iu.test(pathname)
    ? 'public, max-age=31536000, immutable'
    : 'no-cache'
  response.writeHead(200, headers(contentTypes.get(extname(path)) ?? 'application/octet-stream', cacheControl))
  response.end(request.method === 'HEAD' ? undefined : bytes)
})

server.listen(port, host, () => {
  process.stdout.write(`WinWinCode Client listening on http://${host}:${port}\n`)
})
