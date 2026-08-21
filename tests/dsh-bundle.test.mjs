import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { createRequire } from 'node:module'
import {
  cpSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import test from 'node:test'
import vm from 'node:vm'
import { fileURLToPath } from 'node:url'

import {
  composeEntries,
  initProfile,
  loadProfile,
} from '@deepseek-ai/dsh-app-boot'
import { chatSurface } from '../packages/dsh-profile/dist/index.js'
import { strongFlowSurface } from '../packages/strongflow/dist/index.js'

const root = fileURLToPath(new URL('../', import.meta.url))
const require = createRequire(import.meta.url)
const dshPackage = require.resolve('@deepseek-ai/dsh/package.json')
const dshRequire = createRequire(dshPackage)

function copyPublishedPackage(name, destination) {
  const source = join(root, 'packages', name)
  mkdirSync(destination, { recursive: true })
  cpSync(join(source, 'package.json'), join(destination, 'package.json'))
  cpSync(join(source, 'dist'), join(destination, 'dist'), { recursive: true })
  if (name === 'dsh-profile') {
    cpSync(join(source, 'cordis.patch.yml'), join(destination, 'cordis.patch.yml'))
  }
}

function copyInstalledBundle(packageName, destination) {
  const source = dirname(dshRequire.resolve(`${packageName}/package.json`))
  mkdirSync(destination, { recursive: true })
  cpSync(join(source, 'package.json'), join(destination, 'package.json'))
  cpSync(join(source, 'cordis.patch.yml'), join(destination, 'cordis.patch.yml'))
}

function installWorkspacePackages(profileDirectory) {
  const namespace = join(profileDirectory, 'node_modules', '@winwincode')
  mkdirSync(namespace, { recursive: true })
  for (const name of ['contracts', 'native', 'strongflow', 'dsh-profile']) {
    symlinkSync(join(root, 'packages', name), join(namespace, name), 'junction')
  }
}

test('WinWinCode composes as the final layer of a fresh DSH Web profile', () => {
  const home = mkdtempSync(join(tmpdir(), 'winwincode-dsh-profile-'))
  try {
    const installation = join(home, 'installation')
    mkdirSync(installation, { recursive: true })
    const installAnchor = join(installation, 'package.json')
    writeFileSync(installAnchor, `${JSON.stringify({
      name: 'dsh-fixture-installation',
      private: true,
      dependencies: {
        '@deepseek-ai/dsh-base': '0.1.0-rc.8',
        '@deepseek-ai/dsh-web-app': '0.1.0-rc.8',
      },
    }, null, 2)}\n`)
    const deepseekNamespace = join(installation, 'node_modules', '@deepseek-ai')
    copyInstalledBundle('@deepseek-ai/dsh-base', join(deepseekNamespace, 'dsh-base'))
    copyInstalledBundle('@deepseek-ai/dsh-web-app', join(deepseekNamespace, 'dsh-web-app'))

    const profileDirectory = join(home, 'profiles', 'winwincode')
    initProfile(profileDirectory, [
      '@deepseek-ai/dsh-base',
      '@deepseek-ai/dsh-web-app',
      '@winwincode/dsh-profile',
    ])
    const namespace = join(profileDirectory, 'node_modules', '@winwincode')
    copyPublishedPackage('dsh-profile', join(namespace, 'dsh-profile'))
    copyPublishedPackage('strongflow', join(namespace, 'strongflow'))

    const profile = loadProfile(
      'winwincode',
      'winwincode',
      installAnchor,
      home,
    )
    assert.deepEqual(
      profile.layers.map(layer => layer.packageName),
      [
        '@deepseek-ai/dsh-base',
        '@deepseek-ai/dsh-web-app',
        '@winwincode/dsh-profile',
      ],
    )
    assert.equal(
      realpathSync(profile.layers[2].packageDir).startsWith(realpathSync(profileDirectory)),
      true,
    )

    const warnings = []
    const rows = composeEntries(
      [...profile.layers.map(layer => layer.patches), profile.patches],
      warning => warnings.push(warning),
    )
    assert.deepEqual(warnings, [])
    assert.equal(new Set(rows.map(row => row.id)).size, rows.length)
    const byId = new Map(rows.map(row => [row.id, row]))

    assert.equal(byId.get('ui-conversation')?.name, '@deepseek-ai/dsh-client-ui-conversation')
    assert.notEqual(byId.get('ui-conversation')?.disabled, true)
    assert.equal(byId.get('agent-loop')?.disabled, true)
    assert.deepEqual(byId.get('winwincode-agent-factory'), {
      id: 'winwincode-agent-factory',
      name: '@winwincode/dsh-profile/agent-factory',
      config: {
        home: { __jsExpr: "dshHomePath('winwincode')" },
        roleId: 'chat',
      },
    })
    assert.deepEqual(byId.get('winwincode-strongflow'), {
      id: 'winwincode-strongflow',
      name: '@winwincode/strongflow',
    })
    assert.equal(byId.get('agent-presets')?.disabled, true)
    assert.equal(byId.get('ui-agent-preset')?.disabled, true)

    const sourceLock = JSON.parse(
      readFileSync(join(root, 'upstream', 'sources.lock.json'), 'utf8'),
    )
    for (const id of sourceLock.dsh.executionRowsDisabled) {
      assert.equal(byId.get(id)?.disabled, true, `${id} must stay disabled`)
    }
    for (const id of ['approval', 'subagent', 'system-prompt', 'tools']) {
      assert.notEqual(byId.get(id)?.disabled, true, `${id} must stay available`)
    }

    const activeAgentFactories = rows.filter(row => row.disabled !== true && (
      row.name === '@deepseek-ai/dsh-agent-loop'
      || row.name === '@winwincode/dsh-profile/agent-factory'
    ))
    assert.deepEqual(
      activeAgentFactories.map(row => row.name),
      ['@winwincode/dsh-profile/agent-factory'],
    )

    const productPatch = readFileSync(
      join(root, 'packages', 'dsh-profile', 'cordis.patch.yml'),
      'utf8',
    )
    assert.doesNotMatch(productPatch, /(?:api[_-]?key|access[_-]?token|secret)\s*:/iu)
    assert.doesNotMatch(productPatch, /(?:\.codex|node_modules|Volumes\/|Users\/)/u)
  } finally {
    rmSync(home, { force: true, recursive: true })
  }
})

test('DSH chat stays default and the StrongFlow client contributes one opt-in tab', () => {
  assert.equal(chatSurface.default, true)
  assert.equal(strongFlowSurface.default, false)

  let clientRegistration
  vm.runInNewContext(
    readFileSync(join(root, 'packages', 'strongflow', 'dist', 'client.js'), 'utf8'),
    {
      Symbol,
      window: {
        __ModuleLoader__: {
          load(registration) {
            clientRegistration = registration
          },
        },
      },
    },
  )
  assert.equal(clientRegistration?.id, '@winwincode/strongflow')
  const client = clientRegistration.factory(() => {
    throw new Error('StrongFlow requested an undeclared client module')
  })
  assert.deepEqual([...client.inject], ['slots'])

  let slotName
  let slotOptions
  let slotComponent
  client.apply({
    slots: {
      inject(name, register) {
        slotName = name
        register()
      },
      register(options, component) {
        slotOptions = options
        slotComponent = component
      },
    },
  })
  assert.equal(slotName, 'conversation.view')
  assert.deepEqual(
    {
      name: slotOptions.name,
      id: slotOptions.id,
      order: slotOptions.order,
      label: slotOptions.label(),
    },
    {
      name: 'conversation.view',
      id: 'strongflow',
      order: 100,
      label: 'StrongFlow',
    },
  )
  assert.match(slotComponent({ sessionId: 'session-fixture' }), /人工审核/u)
})

test('the real DSH Web tree serves stock Chat and the StrongFlow client without a credential', {
  timeout: 40_000,
}, async () => {
  const home = mkdtempSync(join(tmpdir(), 'winwincode-dsh-boot-'))
  let child
  try {
    const profileDirectory = join(home, 'profiles', 'winwincode')
    initProfile(profileDirectory, [
      '@deepseek-ai/dsh-base',
      '@deepseek-ai/dsh-web-app',
      '@winwincode/dsh-profile',
    ])
    installWorkspacePackages(profileDirectory)

    child = spawn(
      process.execPath,
      [
        join(dirname(dshPackage), 'lib', 'bin.js'),
        '--profile',
        'winwincode',
        '--no-open',
        '--port',
        '0',
      ],
      {
        cwd: root,
        env: {
          ...process.env,
          DSH_HOME: home,
          DSH_TELEMETRY_DISABLED: '1',
        },
        stdio: ['ignore', 'pipe', 'pipe'],
      },
    )
    child.stdout.setEncoding('utf8')
    child.stderr.setEncoding('utf8')
    let stdout = ''
    let stderr = ''
    let resolveUrl
    const urlReady = new Promise(resolve => { resolveUrl = resolve })
    child.stdout.on('data', chunk => {
      stdout += chunk
      const match = stdout.match(/dsh web:\s+(https?:\/\/\S+)/u)
      if (match?.[1] !== undefined) resolveUrl(match[1])
    })
    child.stderr.on('data', chunk => { stderr += chunk })
    const exit = new Promise(resolve => {
      child.once('exit', (code, signal) => resolve({ code, signal }))
    })
    const url = await Promise.race([
      urlReady,
      exit.then(result => {
        throw new Error(`DSH exited before publishing its URL: ${JSON.stringify(result)}\n${stderr}`)
      }),
      new Promise((_, reject) => {
        setTimeout(() => reject(new Error(`DSH startup timed out\n${stderr}`)), 30_000).unref()
      }),
    ])
    const response = await fetch(url)
    assert.equal(response.status, 200)
    assert.match(response.headers.get('content-type') ?? '', /^text\/html/u)
    const html = await response.text()
    assert.match(html, /window\.__DSH_BOOT__/u)
    assert.match(html, /@deepseek-ai\/dsh-client-ui-conversation/u)
    assert.match(html, /@winwincode\/strongflow/u)

    const clientPath = html.match(
      /"id":"@winwincode\/strongflow","url":"([^"]+)"/u,
    )?.[1]
    assert.notEqual(clientPath, undefined)
    const clientResponse = await fetch(new URL(clientPath, url))
    assert.equal(clientResponse.status, 200)
    assert.match(await clientResponse.text(), /@winwincode\/strongflow/u)

    child.kill('SIGTERM')
    const result = await exit
    assert.deepEqual(result, { code: 0, signal: null })
  } finally {
    if (child?.exitCode === null && child.signalCode === null) child.kill('SIGKILL')
    rmSync(home, { force: true, recursive: true })
  }
})
