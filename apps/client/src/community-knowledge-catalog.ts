export type KnowledgeStatus =
  | 'suggested'
  | 'confirmed'
  | 'needs_reconfirmation'
  | 'expired'
  | 'archived'
  | 'revoked'
  | 'tombstone'

export type KnowledgeScope = 'personal' | 'repository'

export interface KnowledgeEntry {
  readonly sourceId: string
  readonly status: KnowledgeStatus
  readonly scope: KnowledgeScope
  readonly title: string | null
  readonly body: string | null
  readonly ruleKey: string | null
  readonly repositoryId: string | null
  readonly sourceVersion: string
  readonly expiresAt: string | null
}

export interface KnowledgeCatalog {
  readonly entries: ReadonlyMap<string, KnowledgeEntry>
}

export interface KnowledgeCaller {
  readonly userId: string
  readonly repositoryIds: ReadonlySet<string>
}

export function emptyKnowledgeCatalog(): KnowledgeCatalog {
  return Object.freeze({ entries: new Map() })
}

export function knowledgeEntersModelContext(status: KnowledgeStatus): boolean {
  return status === 'confirmed'
}

export function upsertKnowledge(
  catalog: KnowledgeCatalog,
  entry: KnowledgeEntry,
): KnowledgeCatalog {
  if (catalog.entries.has(entry.sourceId) && catalog.entries.get(entry.sourceId)?.status === 'tombstone') {
    return catalog
  }
  const next = new Map(catalog.entries)
  const previous = next.get(entry.sourceId)
  let status = entry.status
  if (
    previous !== undefined
    && previous.sourceVersion !== entry.sourceVersion
    && entry.status === 'confirmed'
  ) {
    status = 'needs_reconfirmation'
  }
  if (entry.status === 'confirmed' && previous === undefined) {
    status = 'suggested'
  }
  next.set(entry.sourceId, Object.freeze({ ...entry, status }))
  return Object.freeze({ entries: next })
}

export function confirmKnowledge(
  catalog: KnowledgeCatalog,
  sourceId: string,
): KnowledgeCatalog {
  const entry = catalog.entries.get(sourceId)
  if (
    entry === undefined
    || entry.status === 'tombstone'
    || entry.status === 'expired'
    || entry.status === 'archived'
    || entry.status === 'revoked'
  ) {
    return catalog
  }
  const next = new Map(catalog.entries)
  next.set(sourceId, Object.freeze({ ...entry, status: 'confirmed' as const }))
  return Object.freeze({ entries: next })
}

export function deleteKnowledgeSource(
  catalog: KnowledgeCatalog,
  sourceId: string,
): KnowledgeCatalog {
  const entry = catalog.entries.get(sourceId)
  if (entry === undefined) return catalog
  const next = new Map(catalog.entries)
  next.set(sourceId, Object.freeze({
    sourceId,
    status: 'tombstone' as const,
    scope: entry.scope,
    title: null,
    body: null,
    ruleKey: null,
    repositoryId: entry.repositoryId,
    sourceVersion: entry.sourceVersion,
    expiresAt: null,
  }))
  return Object.freeze({ entries: next })
}

function canRead(entry: KnowledgeEntry, caller: KnowledgeCaller): boolean {
  if (entry.status === 'tombstone') return false
  if (entry.scope === 'personal') {
    return entry.repositoryId === null || caller.repositoryIds.has(entry.repositoryId)
  }
  return entry.repositoryId !== null && caller.repositoryIds.has(entry.repositoryId)
}

export function knowledgeContextEntries(
  catalog: KnowledgeCatalog,
  caller: KnowledgeCaller,
  options: { readonly nowMillis?: number } = {},
): readonly KnowledgeEntry[] {
  const now = options.nowMillis
  const allowed: KnowledgeEntry[] = []
  for (const entry of catalog.entries.values()) {
    if (!canRead(entry, caller)) continue
    if (!knowledgeEntersModelContext(entry.status)) continue
    if (
      entry.expiresAt !== null
      && now !== undefined
      && Date.parse(entry.expiresAt) <= now
    ) {
      continue
    }
    allowed.push(entry)
  }
  return Object.freeze(allowed)
}

export function restoreKnowledgeBackup(input: {
  readonly backup: KnowledgeCatalog
  readonly tombstonesAfterRestorePoint: readonly string[]
  readonly expectedTombstoneIds: readonly string[]
}): { readonly catalog: KnowledgeCatalog; readonly readsOpen: boolean } {
  const provided = new Set(input.tombstonesAfterRestorePoint)
  for (const id of input.expectedTombstoneIds) {
    if (!provided.has(id)) {
      return { catalog: input.backup, readsOpen: false }
    }
  }
  let catalog = input.backup
  for (const sourceId of input.tombstonesAfterRestorePoint) {
    catalog = deleteKnowledgeSource(catalog, sourceId)
  }
  return { catalog, readsOpen: true }
}

export function resolveKnowledgeRule(
  catalog: KnowledgeCatalog,
  caller: KnowledgeCaller,
  ruleKey: string,
): KnowledgeEntry | null {
  const context = knowledgeContextEntries(catalog, caller).filter(
    entry => entry.ruleKey === ruleKey,
  )
  const repository = context.find(entry => entry.scope === 'repository')
  if (repository !== undefined) return repository
  return context.find(entry => entry.scope === 'personal') ?? null
}
