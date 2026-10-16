#!/usr/bin/env node

import { readFileSync, rmSync } from 'node:fs'
import { join } from 'node:path'

export function summarizeWriteErrors(log) {
  const counts = new Map()
  for (const line of log.split('\n')) {
    const offset = line.indexOf('Error executing cache write: ')
    if (offset < 0) continue
    const message = line.slice(offset)
    const status = message.match(/(?:status|status_code|code)[:= ]+(\d{3})\b/iu)?.[1]
    const kind = message.match(/\b(AlreadyExists|RateLimited|PermissionDenied|Unexpected|ConfigInvalid|NotFound)\b/u)?.[1]
    const reason = /already exists|already been reserved|already reserved/iu.test(message) ? 'entry-already-exists'
      : /rate.?limit|too many requests|throttl/iu.test(message) ? 'service-rate-limit'
        : /timed? ?out|timeout/iu.test(message) ? 'timeout'
          : /permission denied|unauthoriz|forbidden/iu.test(message) ? 'access-denied'
            : /reserve cache|reserve_cache|CreateCacheEntry/iu.test(message) ? 'reserve-entry-failed'
              : 'unclassified'
    const label = JSON.stringify({ reason, status: status ?? null, kind: kind ?? null })
    counts.set(label, (counts.get(label) ?? 0) + 1)
  }
  return [...counts].map(([label, count]) => ({ ...JSON.parse(label), count }))
}

if (process.argv[1]?.endsWith('/report-sccache-errors.mjs')) {
  const file = join(process.env.RUNNER_TEMP, 'sccache.log')
  try {
    process.stdout.write(`sccache backend write errors (safe fields only): ${JSON.stringify(summarizeWriteErrors(readFileSync(file, 'utf8')))}\n`)
  } catch (error) {
    process.stdout.write(`sccache diagnostics unavailable (${error.code ?? 'read-failed'})\n`)
  } finally {
    rmSync(file, { force: true })
  }
}
