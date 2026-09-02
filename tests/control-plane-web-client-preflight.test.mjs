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

test('preflight records the current Client, Server, Control Plane, Worker, Local and Helper seams', () => {
  const rules = json(rulesPath)
  assert.equal(rules.schemaVersion, 'winwincode.control-plane-web-client-gate.v2')
  assert.equal(rules.issueId, 'winwincode-9c4.16.6.6.5')
  assert.equal(rules.status, 'implemented-enforced')
  assert.equal(rules.coverage.mode, 'git-file-inventory+rg+ast-grep-fallback')
  assert.equal(rules.coverage.symbolGraphComplete, false)

  for (const path of [
    ...Object.values(rules.architecture),
    rules.generation.generator,
    rules.generation.contractTypes,
    rules.generation.client,
    ...rules.generation.canonicalInputs,
    rules.activation.trigger,
    rules.activation.behaviorProof,
    rules.verification.documentation,
    rules.verification.test,
    ...rules.inventory.client.currentFiles,
    ...rules.inventory.server.paths,
    ...rules.inventory.controlPlane.paths,
    ...rules.inventory.worker.paths,
    ...rules.inventory.local.paths,
    ...rules.inventory.helper.paths,
  ]) assert.equal(existsSync(repositoryPath(path)), true, path)

  assert.equal(rules.inventory.client.transport, 'generated-http-and-websocket-client')
  assert.equal(rules.inventory.client.pageImplementation, true)
  assert.equal(rules.inventory.server.transport, 'Rust HTTP-and-WebSocket-boundary')
  assert.equal(rules.inventory.controlPlane.authority.includes('ProductSession'), true)
  assert.equal(rules.inventory.worker.authority.includes('Lease'), true)
  assert.equal(rules.inventory.local.composition.includes('local process'), true)
  assert.equal(rules.inventory.helper.composition.includes('helper executable'), true)
  assert.equal(
    rules.verification.singlePath,
    'apps/client -> generated client -> winwincode-server -> winwincode-control-plane -> winwincode-worker',
  )

  const contract = readFileSync(contractPath, 'utf8')
  for (const phrase of [
    'implemented-enforced',
    '`apps/client` 是唯一浏览器应用',
    '`winwincode-server` 是唯一公开 HTTP/WebSocket 边界',
    '`winwincode-control-plane` 是 ProductSession、Delivery',
    '`winwincode-worker` 只持有 Job、Lease',
    '`winwincode-local` 负责本地组装',
    '`crates/helper` 只提供经过身份校验的辅助可执行文件',
  ]) assert.equal(contract.includes(phrase), true, phrase)
})

test('generated Client stays anchored to canonical schemas and transport unions', () => {
  const rules = json(rulesPath)
  const http = json(repositoryPath(rules.generation.canonicalInputs[1]))
  const websocket = json(repositoryPath(rules.generation.canonicalInputs[2]))
  const generated = exportedDeclarations(sourceFile(repositoryPath(rules.generation.client)))

  assert.equal(readFileSync(repositoryPath(rules.generation.client), 'utf8').includes(rules.generation.generatedMarker), true)
  for (const expected of rules.generation.requiredClientExports) {
    const actual = generated.get(expected.name)
    assert.ok(actual, `${expected.name} is not a real exported declaration`)
    assert.equal(actual.kind, expected.kind, `${expected.name} has the wrong declaration kind`)
  }
  for (const expected of rules.generation.requiredClientMembers) {
    const declaration = generated.get(expected.export)
    assert.ok(declaration, `${expected.export} is missing`)
    const members = declarationMembers(declaration)
    for (const member of expected.members) {
      assert.ok(members.includes(member), `${expected.export}.${member} is missing`)
    }
  }

  assert.deepEqual(Object.keys(http['x-winwincode-openapi'].paths).sort(), [
    rules.http.authSessionPath,
    rules.http.commandPath,
    rules.http.queryPath,
  ].sort())
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
  assert.deepEqual(rules.websocket.deliveryResetQueries, ['delivery.get', 'runtime.projection.get'])
  assert.deepEqual(rules.websocket.productSessionResetQueries, ['runtime.projection.get'])
  assert.equal(rules.websocket.resetFrameReloadQueries, false)
  assert.equal(
    Object.hasOwn(websocket.$defs.ControlPlaneWebSocketResetRequiredFrame.properties, 'reloadQueries'),
    false,
  )
})

test('Client pages cannot hand-open transports or import Rust runtime authority', () => {
  const rules = json(rulesPath)
  const webRoot = repositoryPath(rules.boundary.webRoot)
  const generatedRoot = repositoryPath(rules.boundary.generatedNetworkOwner)
  const facadePath = repositoryPath(rules.boundary.facade)
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
    if (path !== facadePath) {
      const literals = new Set(stringLiterals(file))
      for (const forbidden of rules.boundary.forbiddenWireLiteralsOutsideGenerated) {
        assert.equal(
          literals.has(forbidden),
          false,
          `${relative(root, path)} hand-maintains wire literal ${forbidden}`,
        )
      }
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

test('generated Client trigger, proof and documentation stay connected', () => {
  const rules = json(rulesPath)
  const trigger = repositoryPath(rules.activation.trigger)
  const source = readFileSync(trigger, 'utf8')
  assert.match(source, /^\/\/ SPDX-License-Identifier: Apache-2\.0\n/u)
  assert.equal(source.includes(rules.generation.generatedMarker), true)

  const imports = importDetails(sourceFile(trigger))
  for (const entry of imports) {
    assert.ok(
      rules.generation.allowedImports.includes(entry.source),
      `generated Client imports non-canonical module ${entry.source}`,
    )
  }
  const generator = readFileSync(repositoryPath(rules.generation.generator), 'utf8')
  assert.equal(generator.includes(rules.activation.trigger.split('/').at(-1)), true)
  const proofSource = sourceFile(repositoryPath(rules.activation.behaviorProof))
  const proofIdentifiers = new Set()
  visit(proofSource, node => {
    if (ts.isIdentifier(node)) proofIdentifiers.add(node.text)
  })
  for (const name of rules.activation.proofUsesExports) {
    assert.ok(proofIdentifiers.has(name), `${rules.activation.behaviorProof} does not exercise ${name}`)
  }

  const contract = readFileSync(contractPath, 'utf8')
  for (const phrase of [
    '`requestId` 和 `expectedRevision`',
    '业务 command 仍走 HTTP',
    'Web 不直接连接 Worker',
    '只公开稳定分类、canonical code、请求 ID',
    'corepack pnpm contracts:check',
    'corepack pnpm verify:source',
  ]) assert.equal(contract.includes(phrase), true, phrase)
})
