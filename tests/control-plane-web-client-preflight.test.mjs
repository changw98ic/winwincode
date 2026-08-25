import assert from 'node:assert/strict'
import {
  existsSync,
  readFileSync,
  readdirSync,
} from 'node:fs'
import { extname, join, relative, resolve, sep } from 'node:path'
import test from 'node:test'

import ts from 'typescript'

const root = resolve(import.meta.dirname, '..')
const rulesPath = join(root, 'docs', 'contracts', 'control-plane-web-client.rules.json')
const contractPath = join(root, 'docs', 'contracts', 'control-plane-web-client.md')

function json(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

function repositoryPath(path) {
  assert.equal(path.startsWith('/'), false, `${path} must be repository-relative`)
  assert.equal(path.split('/').includes('..'), false, `${path} leaves the repository`)
  return join(root, path)
}

function sourceFile(path) {
  const extension = extname(path)
  return ts.createSourceFile(
    path,
    readFileSync(path, 'utf8'),
    ts.ScriptTarget.Latest,
    true,
    extension === '.mjs' || extension === '.js' ? ts.ScriptKind.JS : ts.ScriptKind.TS,
  )
}

function isExported(node) {
  return ts.canHaveModifiers(node)
    && (ts.getModifiers(node) ?? []).some(modifier => modifier.kind === ts.SyntaxKind.ExportKeyword)
}

function exportedDeclarations(file) {
  const declarations = new Map()
  for (const statement of file.statements) {
    if (!isExported(statement)) continue
    if (ts.isClassDeclaration(statement) && statement.name !== undefined) {
      declarations.set(statement.name.text, { kind: 'class', node: statement })
    } else if (ts.isFunctionDeclaration(statement) && statement.name !== undefined) {
      declarations.set(statement.name.text, { kind: 'function', node: statement })
    } else if (ts.isInterfaceDeclaration(statement)) {
      declarations.set(statement.name.text, { kind: 'interface', node: statement })
    } else if (ts.isTypeAliasDeclaration(statement)) {
      declarations.set(statement.name.text, { kind: 'type', node: statement })
    }
  }
  return declarations
}

function declarationMembers(declaration) {
  if (!('members' in declaration.node)) return []
  return declaration.node.members.flatMap(member => {
    if (member.name === undefined) return []
    if (ts.isIdentifier(member.name) || ts.isStringLiteral(member.name)) return [member.name.text]
    return []
  })
}

function importDetails(file) {
  return file.statements.flatMap(statement => {
    if (!ts.isImportDeclaration(statement) || !ts.isStringLiteral(statement.moduleSpecifier)) return []
    const names = []
    const clause = statement.importClause
    if (clause?.name !== undefined) names.push(clause.name.text)
    if (clause?.namedBindings !== undefined && ts.isNamedImports(clause.namedBindings)) {
      names.push(...clause.namedBindings.elements.map(element => element.name.text))
    }
    return [{ source: statement.moduleSpecifier.text, names }]
  })
}

function filesBelow(path) {
  if (!existsSync(path)) return []
  const files = []
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    if (entry.isDirectory() && ['dist', 'node_modules'].includes(entry.name)) continue
    const entryPath = join(path, entry.name)
    if (entry.isDirectory()) files.push(...filesBelow(entryPath))
    else if (entry.isFile()) files.push(entryPath)
  }
  return files
}

function visit(node, callback) {
  callback(node)
  ts.forEachChild(node, child => visit(child, callback))
}

function directNetworkOperations(file) {
  const operations = []
  const expressionName = expression => {
    if (ts.isIdentifier(expression)) return expression.text
    if (ts.isPropertyAccessExpression(expression)) return expression.name.text
    if (ts.isElementAccessExpression(expression)
      && expression.argumentExpression !== undefined
      && ts.isStringLiteral(expression.argumentExpression)) return expression.argumentExpression.text
    return undefined
  }
  visit(file, node => {
    if (ts.isCallExpression(node) && expressionName(node.expression) === 'fetch') {
      operations.push('fetch')
    }
    if (ts.isNewExpression(node)
      && ['EventSource', 'WebSocket', 'XMLHttpRequest'].includes(expressionName(node.expression))) {
      operations.push(expressionName(node.expression))
    }
  })
  return operations
}

function stringLiterals(file) {
  const values = []
  visit(file, node => {
    if (ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) values.push(node.text)
  })
  return values
}

function declaredPropertySets(file) {
  const sets = []
  visit(file, node => {
    const members = ts.isInterfaceDeclaration(node)
      ? node.members
      : (ts.isTypeLiteralNode(node) ? node.members : undefined)
    if (members === undefined) return
    sets.push(new Set(members.flatMap(member => {
      if (member.name === undefined) return []
      if (ts.isIdentifier(member.name) || ts.isStringLiteral(member.name)) return [member.name.text]
      return []
    })))
  })
  return sets
}

function oneOfDiscriminators(schema, definitionName, field) {
  return schema.$defs[definitionName].oneOf.map(branch => {
    const definition = schema.$defs[branch.$ref.split('/').at(-1)]
    const object = definition.allOf?.find(entry => entry.type === 'object') ?? definition
    return object.properties[field].const
  })
}

function tupleConstants(schema, definitionName, field) {
  return schema.$defs[definitionName].properties[field].prefixItems.map(item => (
    item.const ?? schema.$defs[item.$ref.split('/').at(-1)].const
  ))
}

test('preflight records the real current Web, StrongFlow, Chat, and codegen seams', () => {
  const rules = json(rulesPath)
  assert.equal(rules.schemaVersion, 'winwincode.control-plane-web-client-gate.v1')
  assert.equal(rules.issueId, 'winwincode-9c4.16.2.5.4.1')
  assert.equal(rules.status, 'implemented-enforced')
  assert.equal(rules.coverage.mode, 'git-file-inventory+rg+ast-grep-fallback')
  assert.equal(rules.coverage.symbolGraphComplete, false)

  for (const path of [
    rules.generation.generator,
    rules.generation.contractTypes,
    ...rules.generation.canonicalInputs,
    ...rules.inventory.strongFlow.paths,
    ...rules.inventory.chat.paths,
  ]) assert.equal(existsSync(repositoryPath(path)), true, path)

  assert.deepEqual(rules.inventory.web.currentFiles, [
    'apps/web/src/generated/contracts.ts',
    'apps/web/src/generated/control-plane-client.ts',
  ])
  assert.equal(rules.inventory.web.networkImplementation, true)
  assert.equal(rules.inventory.strongFlow.transport, 'dsh-typert-remote')
  assert.equal(rules.inventory.strongFlow.refresh, 'two-second-full-projection-poll')
  assert.equal(
    rules.inventory.strongFlow.restartRecovery,
    'delivery-store-and-runtime-session-ledger-replay',
  )
  assert.equal(rules.inventory.chat.transport, 'stock-dsh-web-app')
  assert.equal(
    rules.inventory.chat.restartRecovery,
    'agent-factory-resume-from-runtime-session-ledger',
  )
  assert.equal(rules.inventory.chat.projectOwnedPageSource, false)
  assert.equal(rules.inventory.web.pageImplementation, false)
  assert.deepEqual(rules.plannedProductCutover, {
    strongFlowUiIssue: 'winwincode-9c4.16.6.3',
    legacyBackendRemovalIssue: 'winwincode-9c4.16.6.6',
    currentPhaseClaim: 'generated-client-only',
  })

  const triggerExists = existsSync(repositoryPath(rules.activation.trigger))
  if (!triggerExists) {
    assert.deepEqual(
      filesBelow(repositoryPath(rules.boundary.webRoot))
        .map(path => relative(root, path).split(sep).join('/'))
        .sort(),
      rules.inventory.web.currentFiles,
    )
  }

  const strongFlow = readFileSync(repositoryPath(rules.inventory.strongFlow.paths[0]), 'utf8')
  assert.match(strongFlow, /remote\.strongflow\.invoke\(/u)
  assert.match(strongFlow, /remote\.strongflow\.advance\(/u)
  assert.match(strongFlow, /POLL_INTERVAL_MILLIS = 2_000/u)
  assert.match(strongFlow, /globalThis\.localStorage/u)
  const chatHost = readFileSync(repositoryPath(rules.inventory.chat.paths[0]), 'utf8')
  assert.match(chatHost, /'@deepseek-ai\/dsh-web-app'/u)
  const deliveryRecovery = readFileSync(
    repositoryPath('packages/dsh-profile/src/delivery-recovery.ts'),
    'utf8',
  )
  assert.match(deliveryRecovery, /RuntimeSessionLedger\.open\(/u)
  assert.match(deliveryRecovery, /projection\.replay\(/u)
  const agentFactory = readFileSync(repositoryPath('packages/dsh-profile/src/agent-factory.ts'), 'utf8')
  assert.match(agentFactory, /async resume\(/u)
  assert.match(agentFactory, /RuntimeSessionLedger\.open\(/u)
  assert.match(agentFactory, /#kernel\.resumeSession\(/u)

  const contract = readFileSync(contractPath, 'utf8')
  assert.match(contract, /winwincode-9c4\.16\.6\.3/u)
  assert.match(contract, /winwincode-9c4\.16\.6\.6/u)
  assert.match(contract, /阶段 2\.5\.4.*只.*生成客户端/u)
})

test('implemented HTTP and WebSocket clients stay anchored to canonical generated types', () => {
  const rules = json(rulesPath)
  const http = json(repositoryPath(rules.generation.canonicalInputs[1]))
  const websocket = json(repositoryPath(rules.generation.canonicalInputs[2]))
  const generated = exportedDeclarations(sourceFile(repositoryPath(rules.generation.contractTypes)))

  for (const name of rules.generation.requiredContractTypes) {
    assert.equal(generated.has(name), true, `${name} is not generated from the canonical schemas`)
  }
  assert.deepEqual(Object.keys(http['x-winwincode-openapi'].paths).sort(), [
    rules.http.commandPath,
    rules.http.queryPath,
  ])
  assert.deepEqual(
    oneOfDiscriminators(websocket, 'ControlPlaneWebSocketClientFrame', 'type').sort(),
    rules.websocket.allowedClientFrames.toSorted(),
  )
  assert.equal(
    oneOfDiscriminators(websocket, 'ControlPlaneWebSocketClientFrame', 'type')
      .some(value => value.includes('command')),
    false,
  )
  assert.deepEqual(rules.http.commandIdentity, ['requestId', 'expectedRevision'])
  assert.deepEqual(rules.strongFlowReset.reloadQueries, [
    'delivery.get',
    'runtime.projection.get',
  ])
  assert.equal(rules.strongFlowReset.publishPartialReload, false)
  assert.equal(rules.strongFlowReset.subscribeBeforeBothQueriesSucceed, false)
  assert.equal(rules.strongFlowReset.expiredCursorError, 'READ_CURSOR_EXPIRED')
  assert.deepEqual(rules.productSessionReset.reloadQueries, ['runtime.projection.get'])
  assert.equal(rules.websocket.resetFrameReloadQueries, false)
  assert.equal(
    Object.hasOwn(websocket.$defs.ControlPlaneWebSocketResetRequiredFrame.properties, 'reloadQueries'),
    false,
  )
  assert.deepEqual(
    tupleConstants(
      websocket,
      'ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent',
      'reloadQueries',
    ),
    ['delivery.get', 'runtime.projection.get'],
  )
  assert.deepEqual(
    tupleConstants(
      websocket,
      'ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent',
      'reloadQueries',
    ),
    ['runtime.projection.get'],
  )
})

test('Web sources cannot hand-open transports or import Worker authority', () => {
  const rules = json(rulesPath)
  const webRoot = repositoryPath(rules.boundary.webRoot)
  const generatedRoot = repositoryPath(rules.boundary.generatedNetworkOwner)
  const sources = filesBelow(webRoot).filter(path => (
    ['.js', '.jsx', '.mjs', '.ts', '.tsx'].includes(extname(path))
  ))

  for (const path of sources) {
    const file = sourceFile(path)
    const imports = importDetails(file)
    for (const entry of imports) {
      for (const forbidden of rules.boundary.forbiddenImportFragments) {
        assert.equal(
          entry.source.includes(forbidden),
          false,
          `${relative(root, path)} imports forbidden ${forbidden}`,
        )
      }
    }
    if (path.startsWith(`${generatedRoot}${sep}`)) continue
    assert.deepEqual(
      directNetworkOperations(file),
      [],
      `${relative(root, path)} bypasses the generated network owner`,
    )
    const literals = new Set(stringLiterals(file))
    for (const forbidden of rules.boundary.forbiddenWireLiteralsOutsideGenerated) {
      assert.equal(
        literals.has(forbidden),
        false,
        `${relative(root, path)} hand-maintains wire literal ${forbidden}`,
      )
    }
    for (const properties of declaredPropertySets(file)) {
      for (const forbiddenShape of rules.boundary.forbiddenTransportPropertySets) {
        assert.equal(
          forbiddenShape.every(property => properties.has(property)),
          false,
          `${relative(root, path)} hand-maintains transport fields ${forbiddenShape.join('+')}`,
        )
      }
    }
  }
})

test('generated client trigger activates structural exports, dependency, and proof gates', () => {
  const rules = json(rulesPath)
  const trigger = repositoryPath(rules.activation.trigger)
  if (!existsSync(trigger)) {
    assert.equal(rules.activation.whenAbsent, 'planned-gated')
    assert.equal(rules.activation.whenPresent, 'enforce-generated-client')
    assert.deepEqual(
      readdirSync(repositoryPath(rules.boundary.generatedNetworkOwner)).sort(),
      ['contracts.ts'],
    )
    return
  }

  const source = readFileSync(trigger, 'utf8')
  assert.match(source, /^\/\/ SPDX-License-Identifier: Apache-2\.0\n/u)
  assert.equal(source.includes(rules.generation.generatedMarker), true)

  const parsed = sourceFile(trigger)
  const declarations = exportedDeclarations(parsed)
  for (const expected of rules.generation.requiredClientExports) {
    const actual = declarations.get(expected.name)
    assert.ok(actual, `${expected.name} is not a real exported declaration`)
    assert.equal(actual.kind, expected.kind, `${expected.name} has the wrong declaration kind`)
  }
  for (const expected of rules.generation.requiredClientMembers) {
    const declaration = declarations.get(expected.export)
    assert.ok(declaration, `${expected.export} is missing`)
    const members = declarationMembers(declaration)
    for (const member of expected.members) {
      assert.ok(members.includes(member), `${expected.export}.${member} is missing`)
    }
  }
  const errorDeclaration = declarations.get('ControlPlaneClientError')
  assert.ok(errorDeclaration)
  const errorMembers = declarationMembers(errorDeclaration).map(member => member.toLowerCase())
  for (const forbidden of rules.errorBoundary.forbiddenFields) {
    assert.equal(
      errorMembers.some(member => member.includes(forbidden.toLowerCase())),
      false,
      `ControlPlaneClientError exposes forbidden ${forbidden}`,
    )
  }

  const imports = importDetails(parsed)
  assert.ok(imports.length > 0, 'generated client must import its canonical generated DTOs')
  for (const entry of imports) {
    assert.ok(
      rules.generation.allowedImports.includes(entry.source),
      `generated client imports non-canonical module ${entry.source}`,
    )
  }
  const importedNames = new Set(imports.flatMap(entry => entry.names))
  for (const name of rules.generation.requiredContractTypes) {
    assert.ok(importedNames.has(name), `generated client does not consume ${name}`)
  }

  const generator = readFileSync(repositoryPath(rules.generation.generator), 'utf8')
  assert.equal(generator.includes(rules.activation.trigger.split('/').at(-1)), true)
  const proofPath = repositoryPath(rules.activation.behaviorProof)
  assert.equal(existsSync(proofPath), true)
  const proofIdentifiers = new Set()
  visit(sourceFile(proofPath), node => {
    if (ts.isIdentifier(node)) proofIdentifiers.add(node.text)
  })
  for (const name of rules.activation.proofUsesExports) {
    assert.ok(proofIdentifiers.has(name), `${rules.activation.behaviorProof} does not exercise ${name}`)
  }
})

test('plain-language contract records the implemented client and its reset split', () => {
  const contract = readFileSync(contractPath, 'utf8')
  for (const phrase of [
    'implemented/enforced',
    '生成文件和可执行行为证明已经存在',
    '`delivery.get` 和 `runtime.projection.get` 都成功',
    '`READ_CURSOR_EXPIRED`',
    '不能凭空补一个 Delivery',
    '`requestId` 和 `expectedRevision`',
    'WebSocket 不提交业务 command',
    'Web 不连接 Execution Worker',
    '只暴露 canonical `ErrorEnvelope` 中已经清理过的错误字段',
  ]) assert.equal(contract.includes(phrase), true, phrase)
})
