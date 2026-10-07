import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const sourceLock = JSON.parse(readFileSync(join(root, 'upstream/sources.lock.json'), 'utf8'))
const manifest = JSON.parse(readFileSync(join(root, 'third_party/codex.UPSTREAM.json'), 'utf8'))
const patches = sourceLock.patches.filter(({ id }) => id.startsWith('codex-code-mode-dispatch-') || id.startsWith('codex-tool-execution-facts-') || id.startsWith('codex-tool-runtime-recovery-') || id.startsWith('codex-tool-diagnostics-') || id.startsWith('codex-tool-dependencies-') || id.startsWith('codex-tool-receipts-') || id.startsWith('codex-tool-runtime-reconciliation-'))
// Measured from P1 commit a3533326af80d7aa0b19bfdf89ba54575dfe1b74.
const originalHashes = {
  "codex-rs/Cargo.lock": "058e337a1f6ffd2abe4e520a70bcecbed51cfa4078e67f14dc8d5fed94953d65",
  "codex-rs/codex-mcp/src/binding.rs": "a6a65c0634c19136c774814154b66b6bc9c32f679d55bc36e7c8560fc55081e7",
  "codex-rs/codex-mcp/src/binding_clients.rs": "6902fce3a1e34922a6964b522a47de4041b8251eefad0504627017fb19f06856",
  "codex-rs/core-api/src/lib.rs": "4b23373a367bad795290f210da5cac8ab50a692e400564e6ba78ea42c08af454",
  "codex-rs/core/Cargo.toml": "bb20965d538df361d1ae039229b2e0da6b36dd9ab0dd07496af6aa2c1a6e588b",
  "codex-rs/core/src/client.rs": "c09c0e28e0991d6c737fa120ccb47cebe980a3d7b15c15b7c3bbf3a3d8bc0f63",
  "codex-rs/core/src/codex_thread.rs": "cac0d5636822ef21c862c407d5ce6f668904147c5c58fec072dafe8b696bf2d4",
  "codex-rs/core/src/context/code_mode_result_unavailable.rs": null,
  "codex-rs/core/src/context/mod.rs": "1a735d45b893efa51693b4658fe02151262aa5e4eaecc15eeb8644ad8d3bbf07",
  "codex-rs/core/src/context/tool_diagnostics.rs": null,
  "codex-rs/core/src/context/tool_diagnostics_tests.rs": null,
  "codex-rs/core/src/context/tool_receipts.rs": null,
  "codex-rs/core/src/context/tool_receipts_tests.rs": null,
  "codex-rs/core/src/context_manager/history_tests.rs": "7dfd7db706ac237be297893868ebfead035e70734c9a8062b55dd514165bb54e",
  "codex-rs/core/src/context_manager/normalize.rs": "4ff15a96885625af1ae398bf1fb28b431e07112d8abb29ba3ce3db7190ff32d4",
  "codex-rs/core/src/lib.rs": "44a094459df2e90521d670734137689d03cb34311842574c827bfb279b78f9ec",
  "codex-rs/core/src/session/handlers.rs": "99c5cb61c9aaced5fb3c847ab8e670ec0eba29117c8eb9ec7336fcc5e991ce06",
  "codex-rs/core/src/session/tests.rs": "ae591efca645992d2bf82417107e930278db7fc6022ce8956d04faaa9d4ad085",
  "codex-rs/core/src/session/turn.rs": "5ab8184951654acd95e84ab741a5d9be31ff192b377bba69b9ab3bfa1fd3cf1a",
  "codex-rs/core/src/session/turn_tests.rs": "f4be2118088914dbe929b95d163256822f81c1413ec85377aac89fa047d707b2",
  "codex-rs/core/src/stream_events_utils.rs": "af42a9b5557dd8c690f0864c5f40fc0402d9b6e33314c2bbba27cc75d19d1ebf",
  "codex-rs/core/src/tasks/mod.rs": "3dde74bfa32aae84fc4489b4729e2dd8845f21e86e6fad8af299d451ce386632",
  "codex-rs/core/src/tool_call_gate.rs": "e575bef5c2c1da71dc470f8881315c51b71f20d07b7ca6ff463ef9f5141fcca4",
  "codex-rs/core/src/tool_input_adapter.rs": null,
  "codex-rs/core/src/tools/authorization.rs": null,
  "codex-rs/core/src/tools/authorization_tests.rs": null,
  "codex-rs/core/src/tools/cell_facts.rs": null,
  "codex-rs/core/src/tools/code_mode/cell_scope.rs": null,
  "codex-rs/core/src/tools/code_mode/cell_scope_tests.rs": null,
  "codex-rs/core/src/tools/code_mode/delegate.rs": "fa82181159e4f1f8de0b480b5e3b88ca772eba34350d6cefed4d8e1d1b1b9ea5",
  "codex-rs/core/src/tools/code_mode/execute_handler.rs": "8097b559c1245574a2b533d933e3d600337da2f85b7bd5c131ac36bc8e2ad88e",
  "codex-rs/core/src/tools/code_mode/mod.rs": "a95017150fa201589f02450faa9c12d232f579536a967fdfc91ca74fb776e229",
  "codex-rs/core/src/tools/code_mode/terminal.rs": null,
  "codex-rs/core/src/tools/code_mode/terminal_tests.rs": null,
  "codex-rs/core/src/tools/code_mode/wait_handler.rs": "dc1e9be66b6df00baa39626a2c697c68e65afaac8db340f9f7916e9efb865b91",
  "codex-rs/core/src/tools/execution_facts.rs": null,
  "codex-rs/core/src/tools/execution_facts_tests.rs": null,
  "codex-rs/core/src/tools/handlers/apply_patch.rs": "3781665d3116941143b786fdcdac8f810d5ee1f7aab24ebd4a3555fe4fe58267",
  "codex-rs/core/src/tools/handlers/current_time.rs": "84d0487ec73730ce344414bc5ea0901dccc476eda0b46469d02356f02ea59648",
  "codex-rs/core/src/tools/handlers/mcp.rs": "9d6a3333c72f22667989036cc5a6495b9cba39c81454519e4dd20f48bc06583b",
  "codex-rs/core/src/tools/handlers/mcp_resource.rs": "e5a199e6b83ca48f02e3808a4e77a751a3a37182eccacb9303de9538f3646266",
  "codex-rs/core/src/tools/handlers/mcp_resource/authorization.rs": null,
  "codex-rs/core/src/tools/handlers/mcp_resource/list_mcp_resource_templates.rs": "c9bb1780bffa9013b69d970298d2c7b92007a7711f404d7dcd5267c221cdc7a0",
  "codex-rs/core/src/tools/handlers/mcp_resource/list_mcp_resources.rs": "cb22c637bc193d416d3447dfbcb6da5281a12d0819e97f7f7eeba68b2fdbc240",
  "codex-rs/core/src/tools/handlers/mcp_resource/read_mcp_resource.rs": "e8766a79ba2f18441c4123b8232d4616c64341b6d48ce536138968fe903f13f4",
  "codex-rs/core/src/tools/handlers/multi_agents/close_agent.rs": "1d0b52093bb0e103c19fad8d53db1b75042756863147fa9fb68d7f06942e5818",
  "codex-rs/core/src/tools/handlers/multi_agents/resume_agent.rs": "d72352c5021116e05a4e1981c6b18da64ec20e398bee7e071874ed2e1f671920",
  "codex-rs/core/src/tools/handlers/multi_agents/send_input.rs": "24a638e9c9806a9e8a8f0ae7e4e92eaae6cfd25bf1228abd1e7b513670b37f6f",
  "codex-rs/core/src/tools/handlers/multi_agents/spawn.rs": "8164118d767555486d76aee36d1fc47511282f7f87bde1c0d5aa1f91fea44425",
  "codex-rs/core/src/tools/handlers/multi_agents/wait.rs": "43e6e8e4f40f6a695c7b27e40327d9643786c0ecad86441258d8af4e2c6ba2bc",
  "codex-rs/core/src/tools/handlers/multi_agents_v2/followup_task.rs": "8982b73d33df01838bd399190845d2e81b402a810210b81799d537b6d229b24c",
  "codex-rs/core/src/tools/handlers/multi_agents_v2/interrupt_agent.rs": "3037707967be6f7c7be969c50f59d9125d7b6916c15da9f556e3f8b2b8ea581f",
  "codex-rs/core/src/tools/handlers/multi_agents_v2/list_agents.rs": "4946efdb345015ed4fe92cdd154ae229beeb3ecda96be35a5044348575cf1ce6",
  "codex-rs/core/src/tools/handlers/multi_agents_v2/send_message.rs": "87765079d320e2a8bdf68cfa1d3fc8aef81d71c4f69f9e7601c4063f9b847d0a",
  "codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs": "1b6d8eb041c5d26152bf76f7ce01b98ccbf6d9e6dd67262ed63b487bb4aebac4",
  "codex-rs/core/src/tools/handlers/multi_agents_v2/wait.rs": "e0def0f64c78cf762da4efd50ad22d331c64d02d7bd0f989401946d1cdb89c0d",
  "codex-rs/core/src/tools/handlers/new_context_window.rs": "93ffc257a98810b98761cb317a38c592d1db32e2148ad77d53a74a6731855042",
  "codex-rs/core/src/tools/handlers/plan.rs": "8ae820eca82be924b7967542a78dc4855d442ebd51b055065b5d8947196bd6b4",
  "codex-rs/core/src/tools/handlers/request_permissions.rs": "cd86267837190a1aec2a03c91a9beb8f11c4178d4f84380ae4fbd6361adc6c3f",
  "codex-rs/core/src/tools/handlers/request_user_input.rs": "b6a982f7b971cd8d7cc22f06854b5b47041c831b9e43721598a0a4dc3393f2e4",
  "codex-rs/core/src/tools/handlers/send_user_message_async.rs": "ec2da964e11b16d214b84b08ad5cfc63fa334547c3984cfe1607189790beff58",
  "codex-rs/core/src/tools/handlers/shell/shell_command.rs": "cb43a1790fb9fb5637cbe243654ffbc13461557364b62d464fe923b6591bc52d",
  "codex-rs/core/src/tools/handlers/sleep.rs": "78d5697a63273604d0e0b5b9cb17e9aa0fa6d8f06bdfd494dc700161f58909b3",
  "codex-rs/core/src/tools/handlers/submit_change_batch.rs": "69ba49d5e3690f2701503c7f71591fcf0db500afac3639deadcccfab86fa5265",
  "codex-rs/core/src/tools/handlers/tool_search.rs": "ce1e8e77f96d708b940b67bd710e4bcc89f1d0f3e5928635e79d7b6d5aae74eb",
  "codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs": "d2a01cff71b251a60266a7e260bbff143a90f3b4a6ef8d52ec52c2a2d52ed047",
  "codex-rs/core/src/tools/handlers/unified_exec/write_stdin.rs": "f9f65fee20f61b8273475c0158007cd2e9bce06a86f85fc48fd7f54e541dc9dc",
  "codex-rs/core/src/tools/handlers/view_image.rs": "5d40e1e8d57d2a6df6df8f6cc3b11b9984483bf0921b93593d429e9690217a84",
  "codex-rs/core/src/tools/mod.rs": "d4ff2d1df97f45444f6f350f6b1b2f5046bd8623e7945f985c37ca1976c35738",
  "codex-rs/core/src/tools/parallel.rs": "95ff9a6b8e4658709aab0b2741b51e7d4adc04f50bdaa839c7fd14ca4ec13b5f",
  "codex-rs/core/src/tools/registry.rs": "0b8a41fceae64696a9406d14c15bc6f2ced930ed3a2e01b456536a3c146b0958",
  "codex-rs/core/src/tools/registry_tests.rs": "ffe2126f122c38f5c6c8e1316ebe5a49bfa4d1be8556de07264172395c405149",
  "codex-rs/core/src/tools/result_recovery.rs": null,
  "codex-rs/core/src/tools/result_recovery_tests.rs": null,
  "codex-rs/core/src/tools/router.rs": "a9fba5bc916ccd19c25ced8fbe06edba0cdc29697485293f2286e0a67bdbd11c",
  "codex-rs/core/src/tools/spec_plan.rs": "eb318ff980fa7c2b4e01bd20aa717c9426d497b5b2f715db401c69ace28b1bd0",
  "codex-rs/core/src/tools/spec_plan_tests.rs": "25bd0ce2761a40825bd0aab4a3420c79f937ec4e183783887dcea85b84ab10f4",
  "codex-rs/core/src/tools/tool_diagnostic_detector.rs": null,
  "codex-rs/core/src/tools/tool_diagnostic_detector_tests.rs": null,
  "codex-rs/core/src/tools/tool_diagnostics.rs": null,
  "codex-rs/core/src/tools/tool_dispatch_trace_tests.rs": "d9219e935b2b14c46b88543bcbdb113c542cd8d38c87a1256d9f4e3f761502f6",
  "codex-rs/core/src/tools/tool_receipts.rs": null,
  "codex-rs/core/src/tools/tool_reconciliation.rs": null,
  "codex-rs/core/src/tools/tool_reconciliation_tests.rs": null,
  "codex-rs/core/src/tools/tool_sharing.rs": null,
  "codex-rs/core/src/tools/tool_sharing_io.rs": null,
  "codex-rs/core/src/tools/tool_sharing_tests.rs": null,
  "codex-rs/core/src/unified_exec/action_admission.rs": null,
  "codex-rs/core/src/unified_exec/mod.rs": "0a18f36d400235ca23a9bddd5f16fc85d60844451c211fb48917c214263183a7",
  "codex-rs/core/src/unified_exec/mod_tests.rs": "ef28473cc6913822d4f308a92a55f2148dfe8c146af59bf671af4fee6e7f9ed7",
  "codex-rs/core/src/unified_exec/process_manager.rs": "a7016c5c321df20a89b85fced47d002fd695e0e88fa690d66fd2dcad9f0e38d6",
  "codex-rs/core/src/unified_exec/recovery.rs": null,
  "codex-rs/core/tests/suite/code_mode.rs": "a27380946930f0404622987e3b23c5ce1478398714b48b5b35d63b79247dd69f",
  "codex-rs/core/tests/suite/mcp_tool_exposure.rs": "02d5aa372f94c8c87974b73381701d119d08f93224cde5debaa52bb842d9a100",
  "codex-rs/core/tests/suite/model_runtime_selectors.rs": "301d554123ef29ee252375e500a9a951d43955add31a21893e1c2e8b38d45ab4",
  "codex-rs/state/migrations/0051_tool_execution_facts.sql": null,
  "codex-rs/state/migrations/0052_tool_runtime_relations.sql": null,
  "codex-rs/state/migrations/0053_tool_diagnostics.sql": null,
  "codex-rs/state/migrations/0054_tool_input_bindings.sql": null,
  "codex-rs/state/migrations/0055_tool_sharing.sql": null,
  "codex-rs/state/src/lib.rs": "0d85f211cfd01993072ec2f4b905a43fddef3f188571cc63fc12572d948366a7",
  "codex-rs/state/src/runtime.rs": "97f6e97b244ed1b4bbadbfc162823551da4e62357992a56fd8c9577351cc7c0d",
  "codex-rs/state/src/runtime/threads.rs": "dee8971f2aaa35efa15b84b6ee22d704e999a3290c59479f18cda10fb6902b54",
  "codex-rs/state/src/runtime/tool_dependencies.rs": null,
  "codex-rs/state/src/runtime/tool_dependencies_tests.rs": null,
  "codex-rs/state/src/runtime/tool_diagnostic_responses.rs": null,
  "codex-rs/state/src/runtime/tool_diagnostics.rs": null,
  "codex-rs/state/src/runtime/tool_diagnostics_tests.rs": null,
  "codex-rs/state/src/runtime/tool_execution.rs": null,
  "codex-rs/state/src/runtime/tool_execution_migration_tests.rs": null,
  "codex-rs/state/src/runtime/tool_execution_tests.rs": null,
  "codex-rs/state/src/runtime/tool_receipts_tests.rs": null,
  "codex-rs/state/src/runtime/tool_reconciliation.rs": null,
  "codex-rs/state/src/runtime/tool_reconciliation_tests.rs": null,
  "codex-rs/state/src/runtime/tool_runtime.rs": null,
  "codex-rs/state/src/runtime/tool_runtime_tests.rs": null,
  "codex-rs/state/src/runtime/tool_sharing.rs": null,
  "codex-rs/state/src/runtime/tool_sharing_tests.rs": null,
  "codex-rs/state/src/tool_dependencies.rs": null,
  "codex-rs/state/src/tool_diagnostics.rs": null,
  "codex-rs/state/src/tool_execution.rs": null,
  "codex-rs/state/src/tool_runtime.rs": null,
  "codex-rs/state/src/tool_sharing.rs": null,
  "codex-rs/v8-poc/Cargo.toml": "e98d307ee1683087debb02cf5fba46cc1a23177a25cd6b03f74f1539239312c9",
  "codex-rs/v8-poc/src/lib.rs": "7121f241941ebc18758bbc9dbfb0848a9ea25a93a65eecd34c890fc86ccecfcc"
}
const digest = bytes => createHash('sha256').update(bytes).digest('hex')

function definitionSections(patch) {
  const strip = patch.stripComponents ?? 1
  assert.equal(Number.isInteger(strip) && strip >= 1, true, patch.file)
  return readFileSync(join(root, patch.file), 'utf8')
    .split(/(?=^diff --git )/mu)
    .filter(section => {
      const path = section.match(/^\+\+\+ (\S+)$/mu)?.[1]
        ?.split('/').slice(strip).join('/')
      if (!Object.hasOwn(originalHashes, path ?? '')) return false
      assert.equal(patch.targets.includes(path), true, `${patch.file}: undeclared ${path}`)
      return true
    })
    .join('')
}

function apply(directory, patch, reverse = false) {
  const input = definitionSections(patch)
  if (!input) return
  const result = spawnSync('patch', [
    '--batch',
    reverse ? '--reverse' : '--forward',
    '--fuzz=0',
    `--strip=${patch.stripComponents ?? 1}`,
    '--directory', directory,
  ], { input, encoding: 'utf8', env: { ...process.env, TMPDIR: tmpdir() } })
  assert.equal(result.error, undefined, patch.file)
  assert.equal(result.status, 0, `${patch.file}\n${result.stdout}\n${result.stderr}`)
}

test('the tool runtime patch stack reproduces every changed dispatch file from the verified P1 baseline', t => {
  assert.equal(sourceLock.codex.commit, '758ef40f50c1a458425c7cfbf1eb12cbc07af0b0')
  assert.equal(sourceLock.codex.archiveSha256, '0413a0e7680bcc2b6c6e998a6ad358115707317ef5d0121dcb9275e88c36121a')
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-code-mode-replay-'))
  t.after(() => rmSync(directory, { force: true, recursive: true }))
  const current = new Map()
  for (const path of Object.keys(originalHashes)) {
    const bytes = readFileSync(join(root, 'third_party/codex', path))
    current.set(path, bytes)
    mkdirSync(dirname(join(directory, path)), { recursive: true })
    writeFileSync(join(directory, path), bytes)
  }
  for (const patch of [...patches].reverse()) apply(directory, patch, true)
  for (const [path, expected] of Object.entries(originalHashes)) {
    if (expected === null) {
      assert.equal(existsSync(join(directory, path)), false, `${path}: new file remains after reverse replay`)
    } else {
      assert.equal(digest(readFileSync(join(directory, path))), expected, `${path}: unrecorded source change`)
    }
  }
  for (const patch of patches) apply(directory, patch)
  for (const [path, bytes] of current) {
    assert.deepEqual(readFileSync(join(directory, path)), bytes, `${path}: replay differs`)
  }
})

test('the tool runtime manifest and ordered patch digests describe the dispatch sources', () => {
  const applied = sourceLock.patches.filter(({ file, planned }) => file.startsWith('upstream/patches/codex/') && !planned)
  assert.deepEqual(manifest.patchesApplied, applied.map(({ file }) => file))
  assert.equal(patches.length, 52)
  assert.deepEqual([...new Set(patches.flatMap(({ targets }) => targets))].sort(), Object.keys(originalHashes).sort())
  for (const patch of patches) {
    assert.equal(digest(readFileSync(join(root, patch.file))), patch.patchSha256, patch.file)
  }
})
