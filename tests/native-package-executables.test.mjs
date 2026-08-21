import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const nativePackages = [
  'packages/native-darwin-arm64',
  'packages/native-darwin-x64',
  'packages/native-linux-arm64',
  'packages/native-linux-x64',
]

function run(command, arguments_, options = {}) {
  const result = spawnSync(command, arguments_, {
    encoding: 'utf8',
    ...options,
  })
  assert.equal(
    result.status,
    0,
    `${command} ${arguments_.join(' ')} failed\n${result.stdout}${result.stderr}`,
  )
  return result.stdout
}

test('native package archives preserve executable helper permissions', t => {
  for (const packageDirectory of nativePackages) {
    const manifest = JSON.parse(readFileSync(join(root, packageDirectory, 'package.json'), 'utf8'))
    const executableFiles = [
      'prebuild/winwincode-kernel-helper',
      ...(manifest.os.includes('linux')
        ? ['prebuild/codex-linux-sandbox', 'prebuild/codex-resources/bwrap']
        : []),
    ]
    const temporaryRoot = mkdtempSync(join(tmpdir(), 'winwincode-native-pack-mode-'))
    t.after(() => rmSync(temporaryRoot, { force: true, recursive: true }))
    const packageRoot = join(temporaryRoot, 'source')
    const archiveRoot = join(temporaryRoot, 'archive')
    const extractedRoot = join(temporaryRoot, 'extracted')
    mkdirSync(packageRoot)
    mkdirSync(archiveRoot)
    mkdirSync(extractedRoot)
    writeFileSync(join(packageRoot, 'package.json'), `${JSON.stringify(manifest, null, 2)}\n`)
    for (const path of executableFiles) {
      const fullPath = join(packageRoot, path)
      mkdirSync(resolve(fullPath, '..'), { recursive: true })
      writeFileSync(fullPath, 'native helper fixture\n')
      chmodSync(fullPath, 0o755)
    }

    const output = run('corepack', [
      'pnpm',
      'pack',
      '--json',
      '--pack-destination',
      archiveRoot,
    ], { cwd: packageRoot })
    const report = JSON.parse(output)
    const filename = report.filename ?? report[0]?.filename
    assert.equal(typeof filename, 'string', `${manifest.name} pack filename`)
    run('tar', ['-xzf', filename, '-C', extractedRoot])

    for (const path of executableFiles) {
      const mode = statSync(join(extractedRoot, 'package', path)).mode & 0o111
      assert.notEqual(mode, 0, `${manifest.name}/${path} is not executable after pnpm pack`)
    }
  }
})
