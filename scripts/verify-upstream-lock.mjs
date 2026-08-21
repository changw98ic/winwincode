#!/usr/bin/env node

import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs'
import { dirname, isAbsolute, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { loadOverlayPatches } from '@deepseek-ai/dsh-app-boot'

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const lockPath = join(repositoryRoot, 'upstream', 'sources.lock.json')
const vendoredCodexRoot = join(repositoryRoot, 'third_party', 'codex')
const codexMetadataPath = join(repositoryRoot, 'third_party', 'codex.UPSTREAM.json')
const errors = []

function fail(message) {
  errors.push(message)
}

function readText(path, label = path) {
  try {
    return readFileSync(path, 'utf8')
  } catch (error) {
    fail(`${label}: ${error.message}`)
    return ''
  }
}

function readJson(path, label = path) {
  const text = readText(path, label)
  if (!text) return undefined
  try {
    return JSON.parse(text)
  } catch (error) {
    fail(`${label}: invalid JSON: ${error.message}`)
    return undefined
  }
}

function parseArgs(argv) {
  const result = {}
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index]
    if (!arg.startsWith('--')) {
      fail(`unexpected argument: ${arg}`)
      continue
    }
    const name = arg.slice(2)
    const value = argv[index + 1]
    if (!value || value.startsWith('--')) {
      fail(`missing value for --${name}`)
      continue
    }
    result[name] = resolve(value)
    index += 1
  }
  return result
}

function ensureArray(value, label) {
  if (!Array.isArray(value)) {
    fail(`${label} must be an array`)
    return []
  }
  return value
}

function ensureUnique(values, label) {
  const duplicates = values.filter((value, index) => values.indexOf(value) !== index)
  if (duplicates.length > 0) fail(`${label} contains duplicates: ${[...new Set(duplicates)].join(', ')}`)
}

function ensureSorted(values, label) {
  const sorted = [...values].sort((left, right) => left.localeCompare(right))
  if (JSON.stringify(values) !== JSON.stringify(sorted)) fail(`${label} must be sorted`)
}

function compareLists(actual, expected, label) {
  const actualSet = new Set(actual)
  const expectedSet = new Set(expected)
  const missing = expected.filter(value => !actualSet.has(value))
  const extra = actual.filter(value => !expectedSet.has(value))
  if (missing.length > 0 || extra.length > 0) {
    fail(`${label} changed; missing=[${missing.join(', ')}], extra=[${extra.join(', ')}]`)
  }
}

function requireFiles(root, paths, label) {
  for (const path of paths) {
    if (!existsSync(join(root, path))) fail(`${label} missing required file: ${path}`)
  }
}

function sha256(path) {
  const hash = createHash('sha256')
  hash.update(readFileSync(path))
  return hash.digest('hex')
}

function verifyArchive(path, expected, label) {
  if (!existsSync(path)) {
    fail(`${label} archive does not exist: ${path}`)
    return
  }
  const actual = sha256(path)
  if (actual !== expected) fail(`${label} archive SHA-256 is ${actual}, expected ${expected}`)
}

function walkPackageJson(root) {
  const result = []
  const stack = [root]
  while (stack.length > 0) {
    const current = stack.pop()
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      if (entry.name === 'node_modules' || entry.name === '.git') continue
      const path = join(current, entry.name)
      if (entry.isDirectory()) stack.push(path)
      else if (entry.isFile() && entry.name === 'package.json') result.push(path)
    }
  }
  return result
}

function profileRows(text) {
  return text
    .split(/\r?\n/u)
    .map(line => /^\s*- id:\s*([^\s#]+)\s*(?:#.*)?$/u.exec(line)?.[1])
    .filter(Boolean)
}

const options = parseArgs(process.argv.slice(2))
options['codex-root'] ??= vendoredCodexRoot
const lock = readJson(lockPath, 'upstream/sources.lock.json')

if (lock) {
  if (lock.schemaVersion !== 1) fail('schemaVersion must be 1')
  if (lock.project?.license !== 'Apache-2.0') fail('project license must be Apache-2.0')
  if (lock.project?.node !== '24.x') fail('project Node version must be 24.x')
  if (lock.project?.pnpm !== '11.7.0') fail('project pnpm version must be 11.7.0')
  if (lock.project?.rust !== '1.95.0') fail('project Rust version must be 1.95.0')

  const expectedTargets = [
    'aarch64-apple-darwin',
    'x86_64-apple-darwin',
    'aarch64-unknown-linux-gnu',
    'x86_64-unknown-linux-gnu',
  ]
  compareLists(ensureArray(lock.project?.targets, 'project.targets'), expectedTargets, 'release targets')

  for (const [name, source] of Object.entries({ codex: lock.codex, dsh: lock.dsh })) {
    if (!source) {
      fail(`missing ${name} source entry`)
      continue
    }
    if (!/^[0-9a-f]{40}$/u.test(source.commit)) fail(`${name}.commit must be a full 40-character SHA`)
    if (!/^[0-9a-f]{64}$/u.test(source.archiveSha256)) fail(`${name}.archiveSha256 must be a SHA-256`)
    if (!source.tag || /^(main|master|latest|head)$/iu.test(source.tag)) fail(`${name}.tag must be immutable`)
    if (!/^https:\/\/github\.com\//u.test(source.repository)) fail(`${name}.repository must be an explicit GitHub URL`)
  }

  const crateList = ensureArray(lock.codex?.productionWorkspaceCrates, 'codex.productionWorkspaceCrates')
  ensureUnique(crateList, 'codex.productionWorkspaceCrates')
  ensureSorted(crateList, 'codex.productionWorkspaceCrates')
  if (!crateList.includes(lock.codex?.publicFacadeCrate)) fail('Codex crate closure must include its public facade')

  const packageList = ensureArray(lock.dsh?.workspacePackages, 'dsh.workspacePackages')
  ensureUnique(packageList, 'dsh.workspacePackages')
  ensureSorted(packageList, 'dsh.workspacePackages')
  for (const root of [...lock.dsh.runtimePackageRoots, ...lock.dsh.buildDependencyRoots]) {
    if (!packageList.includes(root)) fail(`DSH package closure does not include root ${root}`)
  }

  const baseRows = ensureArray(lock.dsh?.baseProfileRows, 'dsh.baseProfileRows')
  const webRows = ensureArray(lock.dsh?.webProfileRows, 'dsh.webProfileRows')
  ensureUnique(baseRows, 'dsh.baseProfileRows')
  ensureUnique(webRows, 'dsh.webProfileRows')
  const knownRows = new Set([...baseRows, ...webRows])
  for (const row of [...lock.dsh.executionRowsReplaced, ...lock.dsh.executionRowsDisabled]) {
    if (!knownRows.has(row)) fail(`DSH execution disposition names unknown row: ${row}`)
  }
  if (!lock.dsh.executionRowsReplaced.includes('agent-loop')) fail('DSH agent-loop must be replaced')
  if (!webRows.includes('ui-conversation')) fail('stock DSH chat UI row must remain inventoried')

  const patches = ensureArray(lock.patches, 'patches')
  for (const patch of patches) {
    if (isAbsolute(patch.file) || patch.file.includes('..')) fail(`patch path must be repository-relative: ${patch.file}`)
    if (!Array.isArray(patch.targets) || patch.targets.length === 0) fail(`patch ${patch.id} must name target files`)
    if (patch.planned !== true && !existsSync(join(repositoryRoot, patch.file))) {
      fail(`applied patch file does not exist: ${patch.file}`)
    }
  }

  const dshProfilePatch = patches.find(patch => patch.id === 'dsh-winwincode-profile')
  if (dshProfilePatch === undefined || dshProfilePatch.planned === true) {
    fail('the WinWinCode DSH profile patch must be recorded as implemented')
  } else {
    try {
      const productPatches = loadOverlayPatches(
        'verify-upstream-lock',
        join(repositoryRoot, dshProfilePatch.file),
      )
      const rowPatches = new Map(
        productPatches
          .filter(patch => typeof patch.id === 'string')
          .map(patch => [patch.id, patch]),
      )
      for (const row of lock.dsh.executionRowsDisabled) {
        if (rowPatches.get(row)?.disabled !== true) {
          fail(`WinWinCode DSH profile does not explicitly disable execution row ${row}`)
        }
      }
      if (rowPatches.get('agent-loop')?.disabled !== true) {
        fail('WinWinCode DSH profile must explicitly disable the stock agent-loop row')
      }
      for (const retained of ['approval', 'subagent', 'system-prompt', 'tools']) {
        if (rowPatches.get(retained)?.disabled === true) {
          fail(`WinWinCode DSH profile disables required retained service ${retained}`)
        }
      }
      const inserted = new Map(
        productPatches.flatMap(patch => patch.insert ?? []).map(row => [row.id, row]),
      )
      if (inserted.get('winwincode-agent-factory')?.name
        !== '@winwincode/dsh-profile/agent-factory') {
        fail('WinWinCode DSH profile does not insert its canonical AgentFactory row')
      }
      if (inserted.get('winwincode-strongflow')?.name !== '@winwincode/strongflow') {
        fail('WinWinCode DSH profile does not insert its canonical StrongFlow row')
      }
    } catch (error) {
      fail(`cannot inspect WinWinCode DSH profile patch: ${error.message}`)
    }
  }

  const codexMetadata = readJson(codexMetadataPath, 'third_party/codex.UPSTREAM.json')
  if (codexMetadata) {
    if (codexMetadata.schemaVersion !== 1) fail('Codex vendored metadata schemaVersion must be 1')
    for (const field of ['repository', 'tag', 'version', 'commit', 'archiveSha256', 'license']) {
      if (codexMetadata[field] !== lock.codex[field]) {
        fail(`Codex vendored metadata ${field} does not match sources.lock.json`)
      }
    }
    const expectedPatches = patches
      .filter(patch => patch.planned !== true && patch.file.startsWith('upstream/patches/codex/'))
      .map(patch => patch.file)
    if (JSON.stringify(codexMetadata.patchesApplied) !== JSON.stringify(expectedPatches)) {
      fail('Codex vendored metadata patch set does not match applied lock entries')
    }
    const vendoredCargoLock = join(vendoredCodexRoot, lock.codex.cargoLock)
    if (existsSync(vendoredCargoLock)) {
      const actualCargoLockHash = sha256(vendoredCargoLock)
      if (actualCargoLockHash !== codexMetadata.cargoLockSha256BeforePatches) {
        fail(
          `vendored Codex Cargo.lock SHA-256 is ${actualCargoLockHash}, `
          + `expected ${codexMetadata.cargoLockSha256BeforePatches}`,
        )
      }
    }
    for (const patchFile of expectedPatches) {
      const patchCheck = spawnSync('patch', [
        '--dry-run',
        '--reverse',
        '--strip=1',
        `--directory=${vendoredCodexRoot}`,
        `--input=${join(repositoryRoot, patchFile)}`,
      ], { encoding: 'utf8' })
      if (patchCheck.status !== 0) {
        fail(
          `vendored Codex does not contain exactly applied patch ${patchFile}: `
          + `${(patchCheck.stderr || patchCheck.stdout).trim()}`,
        )
      }
    }
  }

  const rootCargo = readText(join(repositoryRoot, 'Cargo.toml'), 'Cargo.toml')
  for (const pathDependency of [
    'third_party/codex/codex-rs/arg0',
    'third_party/codex/codex-rs/core-api',
    'third_party/codex/codex-rs/protocol',
  ]) {
    if (!rootCargo.includes(`path = "${pathDependency}"`)) {
      fail(`Cargo.toml does not build embedded Codex path ${pathDependency}`)
    }
  }
  const kernelSource = readText(
    join(repositoryRoot, 'crates', 'kernel', 'src', 'lib.rs'),
    'WinWinCode kernel source',
  )
  for (const patchFile of codexMetadata?.patchesApplied ?? []) {
    if (!kernelSource.includes(`"${patchFile}"`)) {
      fail(`kernel build identity does not expose patch ${patchFile}`)
    }
  }

  const obligations = lock.licenseObligations ?? {}
  if (obligations.projectLicenseOnly !== 'Apache-2.0') fail('projectLicenseOnly must be Apache-2.0')
  for (const name of [
    'preserveCodexLicense',
    'preserveCodexNotice',
    'preserveDshMitText',
    'preserveDshThirdPartyNotices',
    'preserveVendoredLicenses',
    'noDualProjectLicense',
  ]) {
    if (obligations[name] !== true) fail(`license obligation ${name} must be true`)
  }

  if (options['codex-archive']) verifyArchive(options['codex-archive'], lock.codex.archiveSha256, 'Codex')
  if (options['dsh-archive']) verifyArchive(options['dsh-archive'], lock.dsh.archiveSha256, 'DSH')

  if (options['codex-root']) {
    const root = options['codex-root']
    if (!existsSync(root) || !statSync(root).isDirectory()) {
      fail(`Codex root is not a directory: ${root}`)
    } else {
      requireFiles(root, [...lock.codex.noticeFiles, lock.codex.cargoRoot, lock.codex.cargoLock, lock.codex.rustToolchain, ...lock.codex.requiredInterfaces], 'Codex')
      const cargoToml = readText(join(root, lock.codex.cargoRoot), 'Codex Cargo.toml')
      const cargoVersion = /^version\s*=\s*"([^"]+)"/mu.exec(cargoToml)?.[1]
      const cargoLicense = /^license\s*=\s*"([^"]+)"/mu.exec(cargoToml)?.[1]
      if (cargoVersion !== lock.codex.version) fail(`Codex Cargo version is ${cargoVersion}, expected ${lock.codex.version}`)
      if (cargoLicense !== lock.codex.license) fail(`Codex Cargo license is ${cargoLicense}, expected ${lock.codex.license}`)

      const toolchain = readText(join(root, lock.codex.rustToolchain), 'Codex rust-toolchain.toml')
      const channel = /^channel\s*=\s*"([^"]+)"/mu.exec(toolchain)?.[1]
      if (channel !== lock.project.rust) fail(`Codex Rust toolchain is ${channel}, expected ${lock.project.rust}`)

      const notice = readText(join(root, 'NOTICE'), 'Codex NOTICE')
      for (const marker of ['OpenAI Codex', 'Copyright', 'Ratatui']) {
        if (!notice.includes(marker)) fail(`Codex NOTICE is missing marker: ${marker}`)
      }

      const facade = readText(join(root, lock.codex.publicFacadePath), 'Codex public facade')
      for (const symbol of lock.codex.requiredPublicSymbols) {
        if (!new RegExp(`\\b${symbol}\\b`, 'u').test(facade)) fail(`Codex public facade is missing ${symbol}`)
      }
      if (!facade.includes('pub use codex_protocol::mcp::ClientMcpExtensions;')) {
        fail('Codex public facade is missing the applied ClientMcpExtensions export')
      }

      const metadata = spawnSync('cargo', [
        'metadata',
        '--locked',
        '--no-deps',
        '--format-version',
        '1',
        '--manifest-path',
        join(root, lock.codex.cargoRoot),
      ], { encoding: 'utf8' })
      if (metadata.status !== 0) {
        fail(`cargo metadata failed: ${(metadata.stderr || metadata.stdout).trim()}`)
      } else {
        try {
          const parsed = JSON.parse(metadata.stdout)
          const packages = new Map(parsed.packages.map(item => [item.name, item]))
          const closure = new Set()
          const stack = [lock.codex.publicFacadeCrate]
          while (stack.length > 0) {
            const name = stack.pop()
            if (closure.has(name)) continue
            const item = packages.get(name)
            if (!item) {
              fail(`cargo metadata does not contain local crate ${name}`)
              continue
            }
            closure.add(name)
            for (const dependency of item.dependencies) {
              if (dependency.path && dependency.kind !== 'dev' && packages.has(dependency.name)) stack.push(dependency.name)
            }
          }
          compareLists([...closure].sort(), lock.codex.productionWorkspaceCrates, 'Codex production workspace crate closure')
        } catch (error) {
          fail(`cannot parse cargo metadata: ${error.message}`)
        }
      }
    }
  }

  if (options['dsh-root']) {
    const root = options['dsh-root']
    if (!existsSync(root) || !statSync(root).isDirectory()) {
      fail(`DSH root is not a directory: ${root}`)
    } else {
      requireFiles(root, [...lock.dsh.noticeFiles, lock.dsh.pnpmLock, lock.dsh.baseProfile, lock.dsh.webProfile, ...lock.dsh.requiredInterfaces], 'DSH')
      const rootPackage = readJson(join(root, 'package.json'), 'DSH package.json')
      if (rootPackage) {
        if (rootPackage.version !== lock.dsh.version) fail(`DSH version is ${rootPackage.version}, expected ${lock.dsh.version}`)
        if (rootPackage.license !== lock.dsh.license) fail(`DSH license is ${rootPackage.license}, expected ${lock.dsh.license}`)
        if (rootPackage.engines?.node !== lock.dsh.nodeRange) fail(`DSH Node range is ${rootPackage.engines?.node}, expected ${lock.dsh.nodeRange}`)
        if (rootPackage.packageManager !== lock.dsh.packageManager) fail(`DSH package manager is ${rootPackage.packageManager}, expected ${lock.dsh.packageManager}`)
      }

      const dshLicense = readText(join(root, 'LICENSE'), 'DSH LICENSE')
      for (const marker of ['MIT License', 'Copyright (c) 2026 DeepSeek', 'permission notice']) {
        if (!dshLicense.toLowerCase().includes(marker.toLowerCase())) fail(`DSH LICENSE is missing marker: ${marker}`)
      }
      const thirdParty = readText(join(root, 'THIRD_PARTY_NOTICES.md'), 'DSH third-party notices')
      for (const marker of ['Third-Party Notices', 'pnpm-lock.yaml', 'Vendored source']) {
        if (!thirdParty.includes(marker)) fail(`DSH third-party notices are missing marker: ${marker}`)
      }

      const packages = new Map()
      for (const packagePath of walkPackageJson(root)) {
        const item = readJson(packagePath, relative(root, packagePath))
        if (!item?.name) continue
        const existing = packages.get(item.name)
        if (existing && existing.path !== packagePath) {
          fail(`duplicate DSH workspace package name ${item.name}: ${relative(root, existing.path)} and ${relative(root, packagePath)}`)
          continue
        }
        packages.set(item.name, { item, path: packagePath })
      }

      const buildRoots = new Set(lock.dsh.buildDependencyRoots)
      const closure = new Set()
      const stack = [...lock.dsh.runtimePackageRoots]
      while (stack.length > 0) {
        const name = stack.pop()
        if (closure.has(name)) continue
        const entry = packages.get(name)
        if (!entry) {
          fail(`DSH workspace does not contain package ${name}`)
          continue
        }
        closure.add(name)
        const dependencies = {
          ...(entry.item.dependencies ?? {}),
          ...(buildRoots.has(name) ? entry.item.devDependencies ?? {} : {}),
        }
        for (const dependency of Object.keys(dependencies)) {
          if (packages.has(dependency)) stack.push(dependency)
        }
      }
      compareLists([...closure].sort(), lock.dsh.workspacePackages, 'DSH workspace package closure')

      const actualBaseRows = profileRows(readText(join(root, lock.dsh.baseProfile), 'DSH base profile'))
      const actualWebRows = profileRows(readText(join(root, lock.dsh.webProfile), 'DSH web profile'))
      if (JSON.stringify(actualBaseRows) !== JSON.stringify(lock.dsh.baseProfileRows)) fail('DSH base profile row order or membership changed')
      if (JSON.stringify(actualWebRows) !== JSON.stringify(lock.dsh.webProfileRows)) fail('DSH web profile row order or membership changed')

      const interfaceText = lock.dsh.requiredInterfaces
        .map(path => readText(join(root, path), `DSH interface ${path}`))
        .join('\n')
      for (const marker of ['AgentFactory', 'createAgent', 'prepareCall', 'PreparedLlmCall', 'StreamChunk']) {
        if (!interfaceText.includes(marker)) fail(`DSH required interfaces are missing marker: ${marker}`)
      }
    }
  }
}

if (errors.length > 0) {
  console.error('upstream lock verification failed')
  for (const error of errors) console.error(`- ${error}`)
  process.exit(1)
}

console.log(JSON.stringify({
  ok: true,
  lock: relative(repositoryRoot, lockPath),
  verified: {
    structure: true,
    codexSource: Boolean(options['codex-root']),
    dshSource: Boolean(options['dsh-root']),
    codexArchive: Boolean(options['codex-archive']),
    dshArchive: Boolean(options['dsh-archive']),
  },
}, null, 2))
