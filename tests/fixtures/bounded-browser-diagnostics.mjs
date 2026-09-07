const DEFAULT_LIMITS = Object.freeze({
  maxChars: 8_000,
  maxDepth: 2,
  maxObjects: 16,
  maxProperties: 64,
  timeoutMillis: 10_000,
})

function diagnosticText(value, seen = new Set()) {
  if (typeof value === 'string') return value
  if (value === null || typeof value !== 'object' || seen.has(value)) return ''
  seen.add(value)
  return Object.values(value).map(item => diagnosticText(item, seen)).join('\n')
}

function appendDiagnostic(chunks, value, budget, maxChars) {
  const separatorChars = chunks.length === 0 ? 0 : 1
  const remaining = maxChars - budget.chars - separatorChars
  if (remaining <= 0) return false
  const text = diagnosticText(value).slice(0, remaining)
  if (text.length === 0) return true
  chunks.push(text)
  budget.chars += text.length + separatorChars
  return budget.chars < maxChars
}

async function inspectRemoteObject(
  devtools,
  sessionId,
  remoteObject,
  chunks,
  budget,
  seen,
  depth,
  limits,
) {
  if (remoteObject === null || typeof remoteObject !== 'object') {
    appendDiagnostic(chunks, remoteObject, budget, limits.maxChars)
    return
  }
  appendDiagnostic(chunks, {
    description: remoteObject.description,
    type: remoteObject.type,
    unserializableValue: remoteObject.unserializableValue,
    value: remoteObject.value,
  }, budget, limits.maxChars)
  if (
    typeof remoteObject.objectId !== 'string'
    || seen.has(remoteObject.objectId)
    || depth >= limits.maxDepth
    || budget.objects >= limits.maxObjects
    || budget.properties >= limits.maxProperties
    || budget.chars >= limits.maxChars
  ) {
    return
  }
  seen.add(remoteObject.objectId)
  budget.objects += 1
  const properties = await devtools.send('Runtime.getProperties', {
    objectId: remoteObject.objectId,
    ownProperties: true,
    accessorPropertiesOnly: false,
    generatePreview: false,
  }, sessionId, limits.timeoutMillis)
  for (const property of properties.result ?? []) {
    if (
      budget.properties >= limits.maxProperties
      || budget.chars >= limits.maxChars
    ) break
    budget.properties += 1
    appendDiagnostic(chunks, property.name ?? '', budget, limits.maxChars)
    if (property.value !== undefined) {
      await inspectRemoteObject(
        devtools,
        sessionId,
        property.value,
        chunks,
        budget,
        seen,
        depth + 1,
        limits,
      )
    }
  }
}

export async function boundedRemoteObjectText(
  devtools,
  sessionId,
  remoteObject,
  options = {},
) {
  const limits = { ...DEFAULT_LIMITS, ...options }
  for (const value of Object.values(limits)) {
    if (!Number.isSafeInteger(value) || value <= 0) {
      throw new RangeError('browser diagnostic limits must be positive integers')
    }
  }
  const chunks = []
  await inspectRemoteObject(
    devtools,
    sessionId,
    remoteObject,
    chunks,
    { chars: 0, objects: 0, properties: 0 },
    new Set(),
    0,
    limits,
  )
  return chunks.join('\n')
}
