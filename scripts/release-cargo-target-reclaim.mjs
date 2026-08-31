import {
  lstatSync,
  realpathSync,
  rmSync,
  statfsSync,
} from 'node:fs'
import { basename, isAbsolute, relative, resolve, sep } from 'node:path'

export const RELEASE_GATE_BUILD_ROOT_ENV = 'WINWINCODE_RELEASE_GATE_BUILD_ROOT'

export class ReleaseCargoTargetReclaimError extends Error {
  constructor(message) {
    super(message)
    this.name = 'ReleaseCargoTargetReclaimError'
    this.code = 'RELEASE_CARGO_TARGET_RECLAIM_REJECTED'
  }
}

function reject(message) {
  throw new ReleaseCargoTargetReclaimError(message)
}

function strictDescendant(parent, child) {
  const path = relative(parent, child)
  return path !== ''
    && path !== '..'
    && !path.startsWith(`..${sep}`)
    && !isAbsolute(path)
}

function overlaps(left, right) {
  return left === right || strictDescendant(left, right) || strictDescendant(right, left)
}

function directoryIdentity(path, label) {
  let identity
  try {
    identity = lstatSync(path)
  } catch (error) {
    reject(`${label} is unavailable: ${error.message}`)
  }
  if (identity.isSymbolicLink() || !identity.isDirectory()) {
    reject(`${label} must be a real directory`)
  }
  return realpathSync(path)
}

function optionalDirectoryIdentity(path, label) {
  try {
    const identity = lstatSync(path)
    if (identity.isSymbolicLink() || !identity.isDirectory()) {
      reject(`${label} must be a real directory`)
    }
    return realpathSync(path)
  } catch (error) {
    if (error?.code === 'ENOENT') return undefined
    if (error instanceof ReleaseCargoTargetReclaimError) throw error
    reject(`${label} is unavailable: ${error.message}`)
  }
}

function availableBytes(path) {
  const filesystem = statfsSync(path, { bigint: true })
  return filesystem.bavail * filesystem.bsize
}

/**
 * Reclaims only the Cargo target owned by the active release artifact gate.
 * A missing release marker is the normal non-release path and performs no work.
 */
export function reclaimReleaseCargoTarget({ environment = process.env, sourceRoot }) {
  const buildRoot = environment[RELEASE_GATE_BUILD_ROOT_ENV]
  if (buildRoot === undefined) {
    return Object.freeze({ reclaimed: false, reason: 'not-release-gate' })
  }
  const cargoTarget = environment.CARGO_TARGET_DIR
  if (typeof buildRoot !== 'string' || buildRoot.length === 0 || !isAbsolute(buildRoot)) {
    reject(`${RELEASE_GATE_BUILD_ROOT_ENV} must be an absolute path`)
  }
  if (typeof cargoTarget !== 'string' || cargoTarget.length === 0 || !isAbsolute(cargoTarget)) {
    reject('CARGO_TARGET_DIR must be an absolute path during the release gate')
  }
  if (typeof sourceRoot !== 'string' || sourceRoot.length === 0 || !isAbsolute(sourceRoot)) {
    reject('source root must be an absolute path')
  }
  if (!basename(buildRoot).startsWith('winwincode-release-')) {
    reject('release build root does not have the canonical temporary name')
  }
  if (resolve(cargoTarget) !== resolve(buildRoot, 'cargo-target')) {
    reject('CARGO_TARGET_DIR is not the canonical target for this release build root')
  }

  const canonicalSourceRoot = directoryIdentity(sourceRoot, 'source root')
  const canonicalBuildRoot = directoryIdentity(buildRoot, 'release build root')
  if (overlaps(canonicalSourceRoot, canonicalBuildRoot)) {
    reject('release build root must be outside the source tree')
  }
  const canonicalCargoTarget = optionalDirectoryIdentity(cargoTarget, 'release Cargo target')
  if (canonicalCargoTarget === undefined) {
    return Object.freeze({
      reclaimed: false,
      reason: 'cargo-target-absent',
      buildRoot: canonicalBuildRoot,
    })
  }
  if (!strictDescendant(canonicalBuildRoot, canonicalCargoTarget)
    || basename(canonicalCargoTarget) !== 'cargo-target') {
    reject('release Cargo target must be inside this release build root')
  }
  if (overlaps(canonicalSourceRoot, canonicalCargoTarget)) {
    reject('release Cargo target must be outside the source tree')
  }

  const availableBytesBefore = availableBytes(canonicalBuildRoot)
  rmSync(canonicalCargoTarget, { force: true, recursive: true })
  const availableBytesAfter = availableBytes(canonicalBuildRoot)
  return Object.freeze({
    reclaimed: true,
    path: canonicalCargoTarget,
    availableBytesBefore,
    availableBytesAfter,
    availableBytesDelta: availableBytesAfter - availableBytesBefore,
  })
}
