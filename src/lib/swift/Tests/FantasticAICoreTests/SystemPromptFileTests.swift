// The system block's placeholders and its FILE (owner, 2026-10-08) — python
// `ai_core/tests/test_system_prompt_file.py` and rust `tests_prompt_file.rs`, over
// this harness.
//
// Placeholders `{tree}` `{self}` `{store}` `{howto}` are filled per agent and per
// turn in whichever prompt is in force; `{tree}\n\n{self}\n\n{howto}` IS the
// default block. `system_prompt_file: {bridge, path}` makes a file — read THROUGH
// that file_bridge, as the agent — the block, re-read on the next turn after the
// bridge reports it changed. Precedence: per-call > record > file > default.

import FantasticFile
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@testable import FantasticAICore

@Suite("ai_core system_prompt placeholders + system_prompt_file")
struct SystemPromptFileTests {

    private func update(_ kernel: Kernel, _ id: AgentId, _ field: String, _ v: JSON) async -> JSON {
        var p: OrderedDictionary<String, JSON> = ["type": "update_agent", "id": .string(id.value)]
        p[field] = v
        return await kernel.send([Item(AgentId("kernel_state"), .object(p))])[0].result
    }

    /// The mock kernel's running dir, made real on disk (removed by the caller).
    private func workdir(_ kernel: Kernel) -> URL {
        let dir = kernel.workdir!
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// One turn; returns the system block the provider got on pass `n` (1-based).
    private func turn(
        _ kernel: Kernel, _ id: AgentId, _ script: ProviderScript, _ n: Int,
        _ extra: OrderedDictionary<String, JSON> = [:]
    ) async -> String {
        var payload: OrderedDictionary<String, JSON> = [
            "type": "send", "text": "go", "client_id": "c",
        ]
        for (k, v) in extra { payload[k] = v }
        _ = await kernel.send([Item(id, .object(payload))])[0].result
        #expect(await script.waitForCalls(n), "turn \(n) never ran")
        let sys = script.request(n - 1)?.first
        #expect(sys?["role"] == "system")
        return sys?["content"].asString ?? ""
    }

    private func passes(_ n: Int) -> ProviderScript {
        ProviderScript(Array(repeating: ["ok " + doneEnvelope], count: n))
    }

    /// A REAL file_bridge `pf` over the running dir exposing `prompt.md`, granted
    /// both ways for the agent: it reads and watches the file as itself.
    private func wireBridge(_ kernel: Kernel, _ id: AgentId) async -> URL {
        _ = workdir(kernel)
        kernel.bundles.register("fs_bridge.tools", FileBundle())
        let verbs: JSON = ["read_stream", "watch_path", "unwatch_path", "watch"]
        let r = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "fs_bridge.tools", "id": "pf", "root": "",
                    "ingress_rule": "allow_all_ingress", "allow": ["prompt.md"],
                    "callable_by": .object([id.value: verbs]),
                ]))
        ])[0].result
        #expect(r["id"] == "pf", "\(r)")
        var grants = kernel.agent(id)?.metaValue(forKey: "can_call")?.asObject ?? [:]
        grants["pf"] = verbs
        let u = await update(kernel, id, "can_call", .object(grants))
        #expect(u["error"].isNull, "\(u)")
        return kernel.workdir!.appendingPathComponent("prompt.md")
    }

    private func write(_ url: URL, _ text: String) {
        try? Data(text.utf8).write(to: url)
    }

    @Test func theDefaultLayoutInPlaceholdersIsTheDefaultBlock() async throws {
        let script = passes(2)
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        let def = await turn(kernel, id, script, 1)
        _ = await update(kernel, id, "system_prompt", "{tree}\n\n{self}\n\n{howto}")
        #expect(await turn(kernel, id, script, 2) == def)
    }

    @Test func placeholdersFillAndUnknownOnesStay() async throws {
        let script = passes(2)
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        _ = await update(kernel, id, "system_prompt", "store={store} {nope}")
        #expect(await turn(kernel, id, script, 1) == "store=memory {nope}")
        let perCall = await turn(kernel, id, script, 2, ["system_prompt": "me={self}"])
        #expect(perCall.hasPrefix("me=You are `\(id.value)`."), "\(perCall)")
    }

    @Test func theFileIsTheBlockAndAnEditIsLiveNextTurn() async throws {
        let script = passes(12)
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        let prompt = await wireBridge(kernel, id)
        defer { try? FileManager.default.removeItem(at: kernel.workdir!) }
        write(prompt, "FILE-ROLE\n")
        let r = await update(
            kernel, id, "system_prompt_file", ["bridge": "pf", "path": "prompt.md"])
        #expect(r["error"].isNull, "\(r)")
        #expect(await turn(kernel, id, script, 1) == "FILE-ROLE\n")
        try await Task.sleep(nanoseconds: 300_000_000)
        write(prompt, "EDITED {self}\n")
        // the watcher marks it stale; the next turn after that reads it again
        var got = ""
        for n in 2...10 {
            got = await turn(kernel, id, script, n)
            if got.hasPrefix("EDITED") { break }
            try await Task.sleep(nanoseconds: 200_000_000)
        }
        #expect(got.hasPrefix("EDITED You are `\(id.value)`."), "\(got)")
    }

    @Test func aRecordPromptBeatsTheFileAndClearingTheFieldDropsIt() async throws {
        let script = passes(3)
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        let prompt = await wireBridge(kernel, id)
        defer { try? FileManager.default.removeItem(at: kernel.workdir!) }
        write(prompt, "FILE\n")
        _ = await update(kernel, id, "system_prompt_file", ["bridge": "pf", "path": "prompt.md"])
        _ = await update(kernel, id, "system_prompt", "RECORD")
        #expect(await turn(kernel, id, script, 1) == "RECORD")
        _ = await update(kernel, id, "system_prompt", .null)
        #expect(await turn(kernel, id, script, 2) == "FILE\n")
        _ = await update(kernel, id, "system_prompt_file", .null)
        let def = await turn(kernel, id, script, 3)
        #expect(def.contains("You are `\(id.value)`."), "\(def)")
        #expect(PromptFile.errorOf(kernel, id) == nil)
        // the drop released the bridge's hold: no watch task is left
        let ps = await kernel.send([Item(AgentId("kernel"), .object(["type": "ps"]))])[0].result
        #expect(!(ps["tasks"].asArray ?? []).contains { $0["verb"] == "prompt_file" }, "\(ps)")
    }

    @Test func aMissingFileIsTheDefaultBlockAndReflectSaysWhy() async throws {
        let script = passes(12)
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        let prompt = await wireBridge(kernel, id)
        defer { try? FileManager.default.removeItem(at: kernel.workdir!) }
        _ = await update(kernel, id, "system_prompt_file", ["bridge": "pf", "path": "prompt.md"])
        let block = await turn(kernel, id, script, 1)
        #expect(block.contains("You are `\(id.value)`."), "the default block")
        let me = await kernel.send([Item(id, .object(["type": "reflect"]))])[0].result
        #expect(me["system_prompt_file_error"].asString?.contains("prompt.md") == true, "\(me)")
        try await Task.sleep(nanoseconds: 300_000_000)
        write(prompt, "NOW\n")
        var got = ""
        for n in 2...10 {
            got = await turn(kernel, id, script, n)
            if got == "NOW\n" { break }
            try await Task.sleep(nanoseconds: 200_000_000)
        }
        #expect(got == "NOW\n")
        let after = await kernel.send([Item(id, .object(["type": "reflect"]))])[0].result
        #expect(after["system_prompt_file_error"].isNull, "\(after)")
    }

    @Test func anUngrantedBridgeIsTheDefaultBlockAndSaysWhy() async throws {
        let script = passes(1)
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        let id = ids["mock"]!
        kernel.bundles.register("fs_bridge.tools", FileBundle())
        _ = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "fs_bridge.tools", "id": "pf", "root": "",
                    "ingress_rule": "allow_all_ingress", "allow": ["prompt.md"],
                ]))
        ])
        defer { try? FileManager.default.removeItem(at: kernel.workdir!) }
        write(workdir(kernel).appendingPathComponent("prompt.md"), "SECRET\n")
        _ = await update(kernel, id, "system_prompt_file", ["bridge": "pf", "path": "prompt.md"])
        let block = await turn(kernel, id, script, 1)
        #expect(!block.contains("SECRET"), "the agent read a file it was not granted")
        #expect(PromptFile.errorOf(kernel, id)?.contains("watch_path") == true)
    }

    @Test func aBadValueIsRefusedAtTheWrite() {
        // every real backend's `validateMeta` runs the shared schema check
        let sch = RecordSchema.schema([])
        let rec: OrderedDictionary<String, JSON> = ["memory_store_id": "m"]
        for bad: JSON in [
            "prompt.md", ["bridge": "x"], ["bridge": "", "path": "p.md"],
            ["bridge": "x", "path": "p.md", "extra": 1], ["bridge": 5, "path": "p.md"],
        ] {
            let e = RecordSchema.validate(
                sch, backend: "b", meta: ["system_prompt_file": bad], record: rec)
            #expect(e == PromptFile.REFUSAL, "\(bad)")
        }
        #expect(
            RecordSchema.validate(
                sch, backend: "b", meta: ["system_prompt_file": ["bridge": "x", "path": "p.md"]],
                record: rec) == nil)
        #expect(
            RecordSchema.validate(
                sch, backend: "b", meta: ["system_prompt_file": .null], record: rec)
                == nil, "null clears it")
        let names = (sch["fields"].asArray ?? []).compactMap { $0["name"].asString }
        #expect(Array(names.suffix(2)) == ["system_prompt", "system_prompt_file"], "\(names)")
    }
}
