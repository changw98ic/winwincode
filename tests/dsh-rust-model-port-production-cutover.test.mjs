import assert from 'node:assert/strict'
import { readFile, readdir } from 'node:fs/promises'
import { extname, join, resolve } from 'node:path'
import test from 'node:test'

import ts from 'typescript'

const root = resolve(import.meta.dirname, '..')

const productionExecutionSources = Object.freeze([
  'packages/dsh-profile/src/agent-factory.ts',
  'packages/dsh-profile/src/agent-factory-core.ts',
  'packages/native/src/index.ts',
  'crates/native/src/lib.rs',
  'crates/native/src/canonical_model_port.rs',
  'crates/kernel/src/lib.rs',
  'crates/kernel/src/model_port.rs',
])

async function source(path) {
  return readFile(join(root, path), 'utf8')
}

async function filesBelow(directory) {
  const paths = []
  for (const entry of await readdir(join(root, directory), { withFileTypes: true })) {
    const path = join(directory, entry.name)
    if (entry.isDirectory()) {
      if (!['dist', 'node_modules', 'target'].includes(entry.name)) {
        paths.push(...await filesBelow(path))
      }
    } else if (['.md', '.mjs', '.rs', '.ts'].includes(extname(entry.name))) {
      paths.push(path)
    }
  }
  return paths
}

function isProductionInventoryPath(path) {
  return !path.split('/').includes('tests')
}

function injectedPublicFactoryCalls(path, text) {
  const scriptKind = extname(path) === '.ts' ? ts.ScriptKind.TS : ts.ScriptKind.JS
  const file = ts.createSourceFile(path, text, ts.ScriptTarget.Latest, true, scriptKind)
  const calls = []
  function visit(node) {
    if (ts.isNewExpression(node)
      && ts.isIdentifier(node.expression)
      && node.expression.text === 'WinWinCodeAgentFactory'
      && (node.arguments?.length ?? 0) > 2) {
      const { line, character } = file.getLineAndCharacterOfPosition(node.getStart(file))
      calls.push(`${path}:${line + 1}:${character + 1}`)
    }
    ts.forEachChild(node, visit)
  }
  visit(file)
  return calls
}

test('production AgentFactory enters the native Rust model port without DSH callbacks', async () => {
  const [factory, factoryCore, fixture, profileIndex, native, productionModule, fixtureModule]
    = await Promise.all([
    source('packages/dsh-profile/src/agent-factory.ts'),
    source('packages/dsh-profile/src/agent-factory-core.ts'),
    source('packages/dsh-profile/src/agent-factory-test-support.ts'),
    source('packages/dsh-profile/src/index.ts'),
    source('packages/native/src/index.ts'),
    import('../packages/dsh-profile/dist/agent-factory.js'),
    import('../packages/dsh-profile/dist/agent-factory-test-support.js'),
  ])

  assert.doesNotMatch(factory, /\bDshModelPort\b|\bDshLlmRuntime\b|ctx\.llm/u)
  assert.doesNotMatch(factory, /from\s+['"]\.\/model-port\.js['"]/u)
  assert.doesNotMatch(factory, /\bmodelPort\s*:/u)
  assert.doesNotMatch(factory, /export\s+type\s+EmbeddedKernelFactory\b/u)
  assert.doesNotMatch(factory, /createKernel\s*:\s*EmbeddedKernelFactory\b/u)
  assert.match(factory, /constructor\(ctx: Context, config: Config\)/u)
  assert.match(factory, /new WinWinCodeKernel\(options\)/u)
  assert.doesNotMatch(factoryCore, /\bDshModelPort\b|\bDshLlmRuntime\b|ctx\.llm/u)
  assert.doesNotMatch(factoryCore, /from\s+['"]\.\/model-port\.js['"]/u)
  assert.doesNotMatch(factoryCore, /\bmodelPort\s*:/u)
  assert.match(fixture, /WinWinCodeAgentFactoryCore as WinWinCodeAgentFactoryFixture/u)
  assert.doesNotMatch(profileIndex, /export \* from ['"]\.\/model-port\.js['"]/u)
  assert.doesNotMatch(profileIndex, /agent-factory-test-support/u)
  assert.equal(productionModule.WinWinCodeAgentFactory.length, 2)
  assert.equal(Object.hasOwn(productionModule, 'WinWinCodeAgentFactoryFixture'), false)
  assert.equal(fixtureModule.WinWinCodeAgentFactoryFixture.length, 3)
  assert.equal(
    Object.getPrototypeOf(productionModule.WinWinCodeAgentFactory),
    Function.prototype,
  )
  assert.equal(
    Object.getPrototypeOf(productionModule.WinWinCodeAgentFactory.prototype),
    Object.prototype,
  )
  let injected = false
  assert.throws(
    () => Reflect.construct(
      Object.getPrototypeOf(productionModule.WinWinCodeAgentFactory),
      [{}, {}, () => { injected = true }],
    ),
    TypeError,
  )
  assert.equal(injected, false)

  assert.doesNotMatch(native, /\bModelPort(?:Request|Message|Failure|Error)?\b/u)
  assert.doesNotMatch(
    native,
    /#(?:modelPort|modelOperations|openModelStream|pumpModelStream|cancelModelStream)\b/u,
  )
  assert.match(native, /this\.#binding = new binding\.NativeKernel\(nativeOptions\)/u)
})

test('native dependency direction installs the canonical durable ExecutionPort application', async () => {
  const [manifest, native, canonical, nativePackage, profilePackage] = await Promise.all([
    source('crates/native/Cargo.toml'),
    source('crates/native/src/lib.rs'),
    source('crates/native/src/canonical_model_port.rs'),
    source('packages/native/package.json').then(JSON.parse),
    source('packages/dsh-profile/package.json').then(JSON.parse),
  ])

  assert.equal(profilePackage.dependencies['@winwincode/native'], 'workspace:*')
  assert.deepEqual(Object.keys(nativePackage.exports), ['.'])
  assert.equal(Object.hasOwn(profilePackage.exports, './model-port'), false)
  assert.equal(Object.hasOwn(profilePackage.exports, './test-support'), false)
  assert.ok(
    !Object.keys(nativePackage.dependencies).some(dependency => dependency.includes('dsh')),
  )

  for (const dependency of [
    'winwincode-control-plane',
    'winwincode-execution-port',
    'winwincode-storage',
  ]) {
    assert.match(manifest, new RegExp(`^${dependency}(?:\\.workspace)?\\s*=`, 'mu'))
  }
  assert.doesNotMatch(manifest, /^winwincode-(?:cli|worker)\s*=/mu)
  assert.doesNotMatch(
    native,
    /\b(?:ThreadsafeFunction|NativeModelPort|ModelStreamCallback|ModelCancelCallback)\b|model_stream|model_cancel/u,
  )
  assert.match(native, /pub fn new\(options: NativeKernelOptions\) -> Result<Self>/u)
  assert.match(
    native,
    /let model_port = Arc::new\(\s*CanonicalNativeModelPort::open\(&home\)/u,
  )

  assert.match(canonical, /StandaloneModelExecutionApplication::open/u)
  assert.match(canonical, /ExecutionPortMessage::ModelOpenMessage/u)
  assert.match(canonical, /ExecutionPortMessage::ModelAckMessage/u)
  assert.match(canonical, /\.accept_local\(/u)
})

test('legacy DSH model runtime is isolated to its frozen differential sources', async () => {
  const productionInventory = []
  const testSupportInventory = []
  const publicFactoryInjectionAttempts = []
  for (const directory of ['apps', 'crates', 'packages', 'scripts']) {
    for (const path of await filesBelow(directory)) {
      if (!isProductionInventoryPath(path)) continue
      const text = await source(path)
      if (/\bDshModelPort\b|\bDshLlmRuntime\b|['"]\.\/model-port\.js['"]/u.test(text)) {
        productionInventory.push(path)
      }
      if (/\bWinWinCodeAgentFactoryFixture\b|agent-factory-test-support/u.test(text)) {
        testSupportInventory.push(path)
      }
      if (['.js', '.mjs', '.ts'].includes(extname(path))) {
        publicFactoryInjectionAttempts.push(...injectedPublicFactoryCalls(path, text))
      }
    }
  }

  assert.deepEqual(productionInventory.sort(), ['packages/dsh-profile/src/model-port.ts'])
  assert.deepEqual(testSupportInventory.sort(), [
    'packages/dsh-profile/src/agent-factory-test-support.ts',
  ])
  assert.deepEqual(publicFactoryInjectionAttempts, [])
  for (const path of [
    'tests/dsh-model-port.test.mjs',
    'tests/fixtures/native-dsh-model-turn.mjs',
  ]) {
    assert.match(
      await source(path),
      /\bDshModelPort\b|\bDshLlmRuntime\b|['"]\.\/model-port\.js['"]/u,
      path,
    )
  }
})

test('production model execution has no CLI or external-agent process fallback', async () => {
  for (const path of productionExecutionSources) {
    const text = await source(path)
    assert.doesNotMatch(text, /node:child_process|Deno\.Command|Bun\.spawn/u, path)
    assert.doesNotMatch(text, /std::process::(?:Command|Stdio)|Command::new\s*\(/u, path)
    assert.doesNotMatch(
      text,
      /(?:spawn|spawnSync|exec|execFile|Command::new)\s*\([^\n]*(?:codex|agent)/iu,
      path,
    )
  }
})
