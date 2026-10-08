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
const patches = sourceLock.patches.filter(({ id }) => id.startsWith('codex-code-mode-dispatch-') || id.startsWith('codex-tool-execution-facts-') || id.startsWith('codex-tool-runtime-recovery-') || id.startsWith('codex-tool-diagnostics-') || id.startsWith('codex-tool-dependencies-') || id.startsWith('codex-tool-receipts-') || id.startsWith('codex-tool-review-') || id.startsWith('codex-tool-runtime-reconciliation-'))
// Measured from P1 commit a3533326af80d7aa0b19bfdf89ba54575dfe1b74.
const originalHashes = {
  "codex-rs/state/src/sqlite.rs": "005881cb6778e0a68f565accf32d75e2e036a03a59ccf811c3a3b193bccf7f35",
  "MODULE.bazel.lock": "45fcad311f6a2f2ce2bc52ca63e1f05be8e9832e855920e3f89f2cf9938b615f",
  "codex-rs/core/tests/suite/stream_no_completed.rs": "6f1f28b1a4f19e663b4318c3db113abd3b4f8eed1e76922b74b40c5dbb603d6c",
  "codex-rs/Cargo.lock": "058e337a1f6ffd2abe4e520a70bcecbed51cfa4078e67f14dc8d5fed94953d65",
  "codex-rs/app-server/tests/suite/v2/attestation.rs": "37b25c30950628129aa546f3929b80de5f2c7cfa72baaa7fe9d9864756bdddbd",
  "codex-rs/app-server/tests/suite/v2/code_mode_host.rs": "09cd91e4f8c47a1bd48a7641b2af9b5f0f7ce02a70d995418d3f1dbffb0960b4",
  "codex-rs/app-server/tests/suite/v2/imagegen_extension.rs": "e32b57ab4c9541b13903c0220debcb276029de9f0bcf5174a579a10c89156aa1",
  "codex-rs/app-server/tests/suite/v2/mcp_server_elicitation.rs": "f1ecb33e88cf8a98c7baff175ce34c3e79aef8170f992204a4d0e3d228bf0df6",
  "codex-rs/app-server/tests/suite/v2/turn_start.rs": "3d946838f8e58b753a37330918cc183bc79d0791d4164182e5c9f982d5cb6034",
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
  "codex-rs/core/tests/common/responses.rs": "037d19de69ab93731e324562ee873ac851716e2cb5af6ee753aba50b356d1ca5",
  "codex-rs/core/tests/common/test_codex_exec.rs": "eedf3eb43f5568e3a5d14c4cb55d679d380564e3a6841ec0ba0925985e5c0f21",
  "codex-rs/core/tests/suite/agent_execution.rs": "8aefcc225aa95169f527b7cac192b5f022a294448da7d3e0bc139157f4889a44",
  "codex-rs/core/tests/suite/apply_patch_cli.rs": "4ad3ebc23190a6fdb82c0416b7b0ed812bc5433aa7534e22e8fa0dae1c1fa5ba",
  "codex-rs/core/tests/suite/code_mode.rs": "a27380946930f0404622987e3b23c5ce1478398714b48b5b35d63b79247dd69f",
  "codex-rs/core/tests/suite/codex_delegate.rs": "0b6c86bbe4ac9492f2bf763cec4bace5a7c69eb5aabbeebd151685210b76ee1c",
  "codex-rs/core/tests/suite/mcp_tool_exposure.rs": "02d5aa372f94c8c87974b73381701d119d08f93224cde5debaa52bb842d9a100",
  "codex-rs/core/tests/suite/model_runtime_selectors.rs": "301d554123ef29ee252375e500a9a951d43955add31a21893e1c2e8b38d45ab4",
  "codex-rs/core/tests/suite/multi_agent_resume.rs": "fa3e452b96ec3f02f79cafeac3c15e6329bf201a298767f7823f1a78380b92a3",
  "codex-rs/core/tests/suite/send_user_message_async.rs": "f921f899c80f9047fdd377b03df69dc927b87a505393c93214a1cf005c087255",
  "codex-rs/core/tests/suite/skills_extension.rs": "42a74c95549c12cf4f0918dff87b4a16f8dcc91de5e89d88eaff33523739eaac",
  "codex-rs/core/tests/suite/subagent_notifications.rs": "4b9277bd89e66179c50ec2cab17d77b665480ce69055fd7a1b410649ed9a2a2d",
  "codex-rs/core/tests/suite/tool_parallelism.rs": "ed2124f4b31699992d7c1686a303489883de46035dcf792a5caeabb6757831b6",
  "codex-rs/core/tests/suite/unified_exec_process_events.rs": "8ec45892c4f06aa4a0cda6072c878d2762373a4a54dfaca4e306f0b8258ba170",
  "codex-rs/exec/tests/suite/resume.rs": "1926675df9b8d90daf0e66c0a1401a732cdfcdf28ab9876d281fb69dcca454c0",
  "codex-rs/ext/skills/src/host_service_tests.rs": "a71446d59f6475c4736bb4be23dcaadda024dc9ae1e8cd94f1ddcbbca0ce0958",
  "codex-rs/mcp-server/tests/common/mcp_process.rs": "e437b90dac12fda18291c79d326c7e1a41974d7b208d65bf17b1c600ba143fe9",
  "codex-rs/network-proxy/src/http_proxy.rs": "a9c82e4f8142a5072a1cf9e8969c580232f5e0fa1d2bf80f96f7f5203ddd1de6",
  "codex-rs/network-proxy/src/mitm_tests.rs": "1bd9724bec234ef83ec0174c82afc102778b4f1a5224e982af83132f2202de9d",
  "codex-rs/network-proxy/src/network_policy.rs": "87fdcffab34867f3e182ed67983fad3298dbfa3aa640e0f79906d8cd02a11dc2",
  "codex-rs/network-proxy/src/runtime.rs": "3214446d85796e5d1c4a308ba53aab3bf7098fc18cd38cfc6f97a1cbb14d2e4f",
  "codex-rs/network-proxy/src/socks5.rs": "b7580e5aef3ebbfe31fbc3d8901e7316a71b1a17dc44db8dcd4db25765b93c9d",
  "codex-rs/protocol/src/permission_profile_intersection_tests.rs": "762245cece9db9ed4d9b0afe5b303442fc8fe1978af9f7d737b456a1d7b207e1",
  "codex-rs/rmcp-client/src/oauth_http_client_security_tests.rs": "3acfe4d7429fbb667c93b5def4d7b779ff135cfe576ad159717cfacf3e2aed9d",
  "codex-rs/sandboxing/src/seatbelt_tests.rs": "75cde093a6d326b079f9df03fec90188e3ce45e0a74a15cbb698e53f04cb7ad7",
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
  "codex-rs/tui/src/version.rs": "c908ba75a710fb92d84995f8b512f109930501e2cba8f04a33f41b4bd3c9cbb6",
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
  assert.equal(patches.length, 65)
  assert.deepEqual([...new Set(patches.flatMap(({ targets }) => targets))].sort(), Object.keys(originalHashes).sort())
  for (const patch of patches) {
    assert.equal(digest(readFileSync(join(root, patch.file))), patch.patchSha256, patch.file)
  }
})
