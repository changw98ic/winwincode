import { spawnSync } from 'node:child_process'

export function parsePnpmPackReport(output) {
  const report = JSON.parse(output)
  if (report === null || Array.isArray(report) || typeof report !== 'object') {
    throw new TypeError('pnpm pack report must be an object')
  }
  if (!Array.isArray(report.files)) {
    throw new TypeError('pnpm pack report must contain a files array')
  }

  return report.files.map((file, index) => {
    if (
      file === null
      || Array.isArray(file)
      || typeof file !== 'object'
      || typeof file.path !== 'string'
      || file.path.length === 0
    ) {
      throw new TypeError(`pnpm pack report file ${String(index)} must contain a path`)
    }
    return file.path
  })
}

export function pnpmPackDryRun(directory, run = spawnSync) {
  const result = run('corepack', ['pnpm', 'pack', '--dry-run', '--json'], {
    cwd: directory,
    encoding: 'utf8',
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) {
    throw new Error(`pnpm pack failed: ${(result.stderr || result.stdout).trim()}`)
  }
  return parsePnpmPackReport(result.stdout)
}
