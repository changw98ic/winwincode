#!/usr/bin/env node

import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises'
import { basename, isAbsolute, join, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

const packageRoot = resolve(import.meta.dirname, '..')
const repositoryRoot = resolve(packageRoot, '../..')
const manifestName = 'browser-ui-package-release-manifest.json'

export class BrowserUiPackageReleaseError extends Error {
  constructor(code, message) {
    super(message)
    this.name = 'BrowserUiPackageReleaseError'
    this.code = code
  }
}

function outsideRepository(path) {
  const candidate = relative(repositoryRoot, path)
  return candidate === '..' || candidate.startsWith(`..${sep}`) || isAbsolute(candidate)
}

function run(command, args, cwd) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: 'utf8',
    maxBuffer: 16 * 1024 * 1024,
  })
  if (result.error !== undefined) throw result.error
  if (result.status !== 0) {
    throw new BrowserUiPackageReleaseError(
      'BROWSER_UI_PACKAGE_BUILD_FAILED',
      `${command} ${args.join(' ')} failed:\n${result.stdout}${result.stderr}`,
    )
  }
  return result.stdout
}

function digest(bytes) {
  return createHash('sha256').update(bytes).digest('hex')
}

export async function buildBrowserUiPackageRelease(options = {}) {
  if (!options.outputDirectory) {
    throw new BrowserUiPackageReleaseError(
      'BROWSER_UI_PACKAGE_OUTPUT_REQUIRED',
      'an explicit output directory outside the repository is required',
    )
  }
  const outputDirectory = resolve(options.outputDirectory)
  if (!outsideRepository(outputDirectory)) {
    throw new BrowserUiPackageReleaseError(
      'BROWSER_UI_PACKAGE_OUTPUT_INSIDE_REPOSITORY',
      'output directory must be outside the repository',
    )
  }
  await mkdir(outputDirectory, { recursive: true })

  run('corepack', ['pnpm', 'exec', 'tsc', '-p', 'tsconfig.json', '--pretty', 'false'], packageRoot)
  const packed = JSON.parse(run(
    'corepack',
    ['pnpm', 'pack', '--pack-destination', outputDirectory, '--json'],
    packageRoot,
  ))
  const packageManifest = JSON.parse(await readFile(join(packageRoot, 'package.json'), 'utf8'))
  if (packed.name !== packageManifest.name || packed.version !== packageManifest.version) {
    throw new BrowserUiPackageReleaseError(
      'BROWSER_UI_PACKAGE_IDENTITY_MISMATCH',
      'packed npm identity does not match package.json',
    )
  }

  const artifactBytes = await readFile(packed.filename)
  const artifact = {
    fileName: basename(packed.filename),
    bytes: artifactBytes.byteLength,
    sha256: digest(artifactBytes),
  }
  const manifest = {
    schemaVersion: 1,
    kind: 'winwincode.browser-ui-package-release-manifest.v1',
    state: 'package-built-not-published',
    package: {
      name: packageManifest.name,
      version: packageManifest.version,
    },
    artifact,
  }
  const manifestBytes = `${JSON.stringify(manifest, null, 2)}\n`
  const outputPath = join(outputDirectory, manifestName)
  const temporaryPath = join(outputDirectory, `.${manifestName}.${process.pid}.tmp`)
  await writeFile(temporaryPath, manifestBytes, { flag: 'wx' })
  await rename(temporaryPath, outputPath)
  return {
    artifactPath: packed.filename,
    files: packed.files.map(entry => entry.path).sort(),
    manifest,
    manifestPath: outputPath,
  }
}

export function parseArguments(arguments_) {
  if (arguments_.length !== 2 || arguments_[0] !== '--output' || arguments_[1] === undefined) {
    throw new BrowserUiPackageReleaseError(
      'BROWSER_UI_PACKAGE_ARGUMENT_ERROR',
      'usage: build-release.mjs --output DIRECTORY',
    )
  }
  return { outputDirectory: arguments_[1] }
}

export async function runCli(arguments_ = process.argv.slice(2)) {
  try {
    const result = await buildBrowserUiPackageRelease(parseArguments(arguments_))
    process.stdout.write(`${JSON.stringify({
      artifact: result.artifactPath,
      manifest: result.manifestPath,
      sha256: result.manifest.artifact.sha256,
    })}\n`)
    return 0
  } catch (error) {
    const code = error instanceof BrowserUiPackageReleaseError
      ? error.code
      : 'BROWSER_UI_PACKAGE_UNEXPECTED_ERROR'
    process.stderr.write(`${code}: ${error.message}\n`)
    return 1
  }
}

const isDirectExecution = process.argv[1]
  && resolve(process.argv[1]) === fileURLToPath(import.meta.url)
if (isDirectExecution) process.exitCode = await runCli()
