import { spawn } from 'node:child_process'
import {
  lstatSync,
  mkdirSync,
  readFileSync,
  readlinkSync,
  readdirSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs'
import { createRequire } from 'node:module'
import { homedir } from 'node:os'
import { dirname, join, resolve } from 'node:path'

import { initProfile } from '@deepseek-ai/dsh-app-boot'

export const WINWINCODE_DSH_PROFILE = 'winwincode'
export const WINWINCODE_DSH_BUNDLES = Object.freeze([
  '@deepseek-ai/dsh-base',
  '@deepseek-ai/dsh-web-app',
  '@winwincode/dsh-profile',
] as const)

export interface WinWinCodeWebOptions {
  readonly cwd?: string
  readonly env?: NodeJS.ProcessEnv
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function packageRoot(packageName: string): string {
  return dirname(createRequire(import.meta.url).resolve(`${packageName}/package.json`))
}

function dshPackageRoot(packageName: string): string {
  const manifest = createRequire(import.meta.url).resolve('@deepseek-ai/dsh/package.json')
  return dirname(createRequire(manifest).resolve(`${packageName}/package.json`))
}

function ensureSymlink(link: string, target: string): void {
  let existing: ReturnType<typeof lstatSync> | undefined
  try {
    existing = lstatSync(link)
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error
  }
  if (existing !== undefined) {
    if (!existing.isSymbolicLink()) {
      throw new Error(`${link} exists and is not a WinWinCode-managed package link`)
    }
    if (readlinkSync(link) === target) return
    unlinkSync(link)
  }
  try {
    symlinkSync(target, link, 'junction')
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== 'EEXIST'
      || !lstatSync(link).isSymbolicLink()
      || readlinkSync(link) !== target) throw error
  }
}

function normalizeProfileManifest(profileDirectory: string): void {
  const path = join(profileDirectory, 'package.json')
  const parsed: unknown = JSON.parse(readFileSync(path, 'utf8'))
  if (!isRecord(parsed)) throw new Error(`${path} must contain a JSON object`)
  const dsh = isRecord(parsed.dsh) ? parsed.dsh : {}
  const profile = isRecord(dsh.profile) ? dsh.profile : {}
  const current = Array.isArray(profile.bundles) ? profile.bundles : []
  if (current.length === WINWINCODE_DSH_BUNDLES.length
    && current.every((value, index) => value === WINWINCODE_DSH_BUNDLES[index])) return
  writeFileSync(path, `${JSON.stringify({
    ...parsed,
    dsh: {
      ...dsh,
      profile: {
        ...profile,
        bundles: [...WINWINCODE_DSH_BUNDLES],
      },
    },
  }, null, 2)}\n`)
}

/** Resolve the one durable home shared by the installed DSH and Delivery CLI. */
export function resolveWinWinCodeHome(environment: NodeJS.ProcessEnv = process.env): string {
  return join(resolve(environment.DSH_HOME ?? join(homedir(), '.dsh')), WINWINCODE_DSH_PROFILE)
}

/** Create or normalize the installed WinWinCode profile without copying package code into it. */
export function ensureWinWinCodeProfile(
  environment: NodeJS.ProcessEnv = process.env,
): string {
  const dshHome = resolve(environment.DSH_HOME ?? join(homedir(), '.dsh'))
  const profileDirectory = join(dshHome, 'profiles', WINWINCODE_DSH_PROFILE)
  initProfile(profileDirectory, [...WINWINCODE_DSH_BUNDLES])
  normalizeProfileManifest(profileDirectory)
  const namespace = join(dshHome, 'profiles', 'node_modules', '@winwincode')
  mkdirSync(namespace, { recursive: true })
  for (const packageName of ['@winwincode/dsh-profile', '@winwincode/strongflow']) {
    ensureSymlink(join(namespace, packageName.slice('@winwincode/'.length)), packageRoot(packageName))
  }
  const deepseekNamespace = join(dshHome, 'profiles', 'node_modules', '@deepseek-ai')
  mkdirSync(deepseekNamespace, { recursive: true })
  for (const bundle of ['@deepseek-ai/dsh-base', '@deepseek-ai/dsh-web-app']) {
    const installedNamespace = dirname(dshPackageRoot(bundle))
    for (const name of readdirSync(installedNamespace)) {
      ensureSymlink(join(deepseekNamespace, name), join(installedNamespace, name))
    }
  }
  return profileDirectory
}

/** Start the stock DSH Web host with the WinWinCode profile as its final layer. */
export async function runWinWinCodeWeb(
  args: readonly string[],
  options: WinWinCodeWebOptions = {},
): Promise<number> {
  const environment = { ...process.env, ...options.env }
  const dshHome = resolve(environment.DSH_HOME ?? join(homedir(), '.dsh'))
  environment.DSH_HOME = dshHome
  ensureWinWinCodeProfile(environment)
  const dshPackage = createRequire(import.meta.url).resolve('@deepseek-ai/dsh/package.json')
  const child = spawn(process.execPath, [
    join(dirname(dshPackage), 'lib', 'bin.js'),
    '--profile',
    WINWINCODE_DSH_PROFILE,
    ...args,
  ], {
    cwd: options.cwd ?? process.cwd(),
    env: environment,
    stdio: 'inherit',
  })
  return new Promise((resolvePromise, reject) => {
    let interruptedBy: 'SIGINT' | 'SIGTERM' | undefined
    const forwardSignal = (signal: 'SIGINT' | 'SIGTERM'): void => {
      interruptedBy ??= signal
      if (child.exitCode === null && child.signalCode === null) child.kill(signal)
    }
    const sigint = (): void => forwardSignal('SIGINT')
    const sigterm = (): void => forwardSignal('SIGTERM')
    process.once('SIGINT', sigint)
    process.once('SIGTERM', sigterm)
    const cleanup = (): void => {
      process.removeListener('SIGINT', sigint)
      process.removeListener('SIGTERM', sigterm)
    }
    child.once('error', (error) => {
      cleanup()
      reject(error)
    })
    child.once('exit', (code, signal) => {
      cleanup()
      if (interruptedBy !== undefined) {
        resolvePromise(interruptedBy === 'SIGINT' ? 130 : 143)
        return
      }
      if (code !== null) resolvePromise(code)
      else if (signal === 'SIGINT') resolvePromise(130)
      else if (signal === 'SIGTERM') resolvePromise(143)
      else resolvePromise(1)
    })
  })
}
