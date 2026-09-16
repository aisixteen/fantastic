// static_loader — declarative YAML roster hydration + the deny-all closure.
// Mirror of python `test_static_loader.py` and rust `fantastic-static-loader`.

import FantasticFile
import FantasticJSON
import FantasticKernel
import FantasticKernelStartup
import Foundation
import Testing

@testable import FantasticStaticLoader

private let MANIFEST = """
    agents:
      - id: memory
        handler_module: cli.tools
      - id: worker
        handler_module: cli.tools
        can_call:
          memory: [set, read]
    """

private func mkKernel() -> Kernel {
    let kernel = Kernel(storage: .inMemory, bundles: defaultBundleRegistry())
    let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

/// A manifest UNDER THE RUNNING DIR, and the RELATIVE path to it.
///
/// Under, because the manifest read obeys the running-dir law: `fs` is the one
/// disk surface and the cold bring-up read is not exempt, so a manifest in the
/// system temp dir is refused (python and rust behave the same). These tests
/// cannot chdir — sibling suites share the process — so the file comes to the
/// cwd instead of the cwd going to it, exactly as rust's loader tests do.
///
/// Returned with a cleanup closure: a leaked fixture is a directory that
/// reappears in `git status` on every run.
private func writeManifest(_ text: String) -> (path: String, cleanup: () -> Void) {
    let name = ".sl_fixture_\(UUID().uuidString.prefix(8))"
    let dir = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        .appendingPathComponent(name)
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let file = dir.appendingPathComponent("fantastic.yaml")
    try? text.write(to: file, atomically: true, encoding: .utf8)
    return ("\(name)/fantastic.yaml", { try? FileManager.default.removeItem(at: dir) })
}

/// The fixture outlives this call: a test that re-boots the loader needs the
/// manifest still on disk, so the CALLER owns the cleanup.
private func bootLoader(_ text: String) async -> (Kernel, () -> Void) {
    let kernel = mkKernel()
    let (path, cleanup) = writeManifest(text)
    // Operator send (top-level → ungated); create_agent auto-boots the loader,
    // which hydrates the roster.
    let rec = await kernel.send([
        Item(
            "core",
            [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": .string(StaticLoaderBundle.handlerModule),
                "id": "static_loader",
                "manifest": .string(path),
            ])
    ])[0].result
    #expect(rec["id"].asString == "static_loader", "loader create: \(rec.serialize())")
    return (kernel, cleanup)
}

@Test func rosterIsHydratedAsChildrenOfTheLoader() async {
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
    #expect(kernel.agent(AgentId("memory")) != nil)
    #expect(kernel.agent(AgentId("worker")) != nil)
    for id in ["memory", "worker"] {
        #expect(kernel.agent(AgentId(id))?.parentId?.value == "static_loader")
    }
}

@Test func declaredAgentsCarryBaselinePlusExplicitGrants() async {
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
    let can = kernel.agent(AgentId("worker"))?.metaValue(forKey: "can_call") ?? .null
    #expect(can["$self"].asString == "*")
    #expect(can["$parent"].asString == "*")
    #expect(can["$children"].asString == "*")
    #expect(can["memory"].asArray?.compactMap { $0.asString } == ["set", "read"])
    // memory got the baseline with no explicit can_call in the manifest.
    let mem = kernel.agent(AgentId("memory"))?.metaValue(forKey: "can_call") ?? .null
    #expect(mem["$self"].asString == "*")
}

@Test func rosterIsClosedAChildCannotCreateMore() async {
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
    // deny-all: a roster agent is not granted create_agent on anyone.
    let r = await kernel.sendAs(
        sender: "worker", target: "core",
        payload: ["type": "create_agent", "parent_id": "core", "handler_module": "cli.tools"])
    #expect(r["reason"].asString == "unauthorized", "\(r.serialize())")
}

@Test func bootIsIdempotentById() async {
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
    let before = kernel.allAgents().count
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(r["booted"].asBool == true, "\(r.serialize())")
    let reapplied = Set((r["reapplied"].asArray ?? []).compactMap { $0.asString })
    #expect(reapplied == ["memory", "worker"])
    #expect((r["created"].asArray ?? []).isEmpty)
    #expect(kernel.allAgents().count == before)
}

/// A8.8 / A8.12 — the manifest read is `fs.resolve(cwd, path)`: the error tail
/// is python's own exception text, and a path outside the running dir (or an
/// ABSOLUTE one, even inside it) escapes.
@Test func anUnreadableManifestIsRefusedAtTheWRITE() async {
    // A broken roster never becomes a booting record: `validateMeta` reads the
    // manifest at create/update time, so the refusal lands on the WRITE and no
    // agent is made. This runtime used to accept the record and only complain
    // at boot — python and rust both refused it here, and the shared agentic
    // corpus is what surfaced the difference (2026-08-28).
    //
    // The path is also outside the running dir, which is a second reason to
    // refuse it: the manifest read is clamped, cold bring-up included.
    let kernel = mkKernel()
    let before = kernel.allAgents().count
    let rec = await kernel.send([
        Item(
            "core",
            [
                "type": "create_agent",
                "parent_id": "core",
                "handler_module": .string(StaticLoaderBundle.handlerModule),
                "id": "sl2", "manifest": "/no/such/path/nope.yaml",
            ])
    ])[0].result
    #expect(rec["reason"].asString == "invalid_meta", "\(rec.serialize())")
    #expect(
        rec["error"].asString
            == "static_loader: manifest '/no/such/path/nope.yaml' is not readable: "
            + "fs: path '/no/such/path/nope.yaml' escapes root", "\(rec.serialize())")
    #expect(kernel.agent("sl2") == nil, "a doomed record was created anyway")
    #expect(kernel.allAgents().count == before)
    // A MISSING relative manifest names the RESOLVED path, python's OSError str.
    let cwd = FileManager.default.currentDirectoryPath
    let miss = await kernel.send([
        Item(
            "core",
            [
                "type": "create_agent", "parent_id": "core",
                "handler_module": .string(StaticLoaderBundle.handlerModule),
                "id": "sl3", "manifest": "nope.yaml",
            ])
    ])[0].result
    #expect(
        miss["error"].asString
            == "static_loader: manifest 'nope.yaml' is not readable: "
            + "[Errno 2] No such file or directory: \(pyRepr(pyRealpath(cwd) + "/nope.yaml"))",
        "\(miss.serialize())")
}

/// A8.12 — an ABSOLUTE manifest is refused even when it points INSIDE the
/// running dir: python's `fs.resolve` never re-roots a leading `/`.
@Test func anAbsoluteManifestInsideCwdIsRefused() async {
    let kernel = mkKernel()
    let (rel, cleanup) = writeManifest(MANIFEST)
    defer { cleanup() }
    let abs = FileManager.default.currentDirectoryPath + "/" + rel
    let rec = await kernel.send([
        Item(
            "core",
            [
                "type": "create_agent", "parent_id": "core",
                "handler_module": .string(StaticLoaderBundle.handlerModule),
                "id": "sl_abs", "manifest": .string(abs),
            ])
    ])[0].result
    #expect(
        rec["error"].asString
            == "static_loader: manifest \(pyRepr(abs)) is not readable: "
            + "fs: path \(pyRepr(abs)) escapes root", "\(rec.serialize())")
}

/// A8.12 — a symlink inside the running dir that points OUT of it is followed
/// and refused (python resolves before it compares).
@Test func aSymlinkedManifestEscapingCwdIsRefused() async throws {
    let kernel = mkKernel()
    let fm = FileManager.default
    let name = ".sl_link_\(UUID().uuidString.prefix(8)).yaml"
    let link = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(name)
    try fm.createSymbolicLink(at: link, withDestinationURL: URL(fileURLWithPath: "/etc/hosts"))
    defer { try? fm.removeItem(at: link) }
    let rec = await kernel.send([
        Item(
            "core",
            [
                "type": "create_agent", "parent_id": "core",
                "handler_module": .string(StaticLoaderBundle.handlerModule),
                "id": "sl_link", "manifest": .string(name),
            ])
    ])[0].result
    #expect(
        rec["error"].asString
            == "static_loader: manifest \(pyRepr(name)) is not readable: "
            + "fs: path \(pyRepr(name)) escapes root", "\(rec.serialize())")
}

/// A8.1 / A8.2 — an unknown verb (and `shutdown`, which is not a verb here) is
/// the coded refusal python's `handler` builds.
@Test func anUnknownVerbIsACodedRefusal() async {
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
    for verb in ["frobnicate", "shutdown"] {
        let r = await kernel.send([Item("static_loader", ["type": .string(verb)])])[0].result
        #expect(r["reason"].asString == "unhandled", "\(verb) → \(r.serialize())")
        #expect(
            r["error"].asString == "static_loader: unknown type \(pyRepr(verb))",
            "\(r.serialize())")
        #expect(r["verb"].asString == verb, "\(r.serialize())")
    }
}

/// A8.4 — a record with no manifest boots with python's text (`{'manifest'!r}`).
@Test func aRecordWithoutAManifestBootsWithPythonsText() async {
    let kernel = mkKernel()
    let rec = await kernel.send([
        Item(
            "core",
            [
                "type": "create_agent", "parent_id": "core",
                "handler_module": .string(StaticLoaderBundle.handlerModule), "id": "bare",
            ])
    ])[0].result
    #expect(rec["id"].asString == "bare", "\(rec.serialize())")
    let b = await kernel.send([Item("bare", ["type": "boot"])])[0].result
    #expect(
        b["error"].asString == "static_loader: no 'manifest' path on the record",
        "\(b.serialize())")
    #expect(b["booted"].asBool == false, "\(b.serialize())")
}

/// A8.9 — a python-FALSY YAML document is an empty mapping and boots empty; a
/// truthy non-mapping is refused.
@Test func aFalsyYamlDocumentBootsEmpty() async {
    for text in ["", "~", "0", "false", "[]", "{}"] {
        let (kernel, cleanup) = await bootLoader(text)
        defer { cleanup() }
        let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
        #expect(r["booted"].asBool == true, "\(text) → \(r.serialize())")
        #expect(
            (r["created"].asArray ?? []).isEmpty && (r["reapplied"].asArray ?? []).isEmpty,
            "\(text) → \(r.serialize())")
    }
    let (kernel, cleanup) = await bootLoader("[1]")
    defer { cleanup() }
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(
        r["error"].asString?.hasSuffix("must be a mapping with `agents:`") == true,
        "\(r.serialize())")
}

/// A8.21 — `agents:` present but not a list is refused (python `agents: null`
/// takes the same path: `None` is not a list).
@Test func agentsPresentButNotAListIsRefused() async {
    for doc in ["agents: 5", "agents:"] {
        let (kernel, cleanup) = await bootLoader(doc)
        defer { cleanup() }
        let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
        #expect(r["booted"].asBool == false, "\(doc) → \(r.serialize())")
        #expect(
            r["error"].asString?.hasPrefix("static_loader: `agents:` in ") == true
                && r["error"].asString?.hasSuffix(" must be a list") == true,
            "\(doc) → \(r.serialize())")
    }
}

/// A8.10 — `handler_module` absent or FALSY is refused with the entry's python
/// dict repr; a truthy non-string passes the check and fails at create.
@Test func handlerModuleAbsentOrFalsyIsRefusedTruthyPasses() async {
    for (doc, repr) in [
        ("agents:\n  - id: a\n", "{'id': 'a'}"),
        ("agents:\n  - id: a\n    handler_module: ''\n", "{'id': 'a', 'handler_module': ''}"),
        ("agents:\n  - id: a\n    handler_module: 0\n", "{'id': 'a', 'handler_module': 0}"),
    ] {
        let (kernel, cleanup) = await bootLoader(doc)
        defer { cleanup() }
        let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
        #expect(
            r["error"].asString
                == "static_loader: each roster entry needs a handler_module: \(repr)",
            "\(doc) → \(r.serialize())")
    }
    let (kernel, cleanup) = await bootLoader("agents:\n  - id: a\n    handler_module: 5\n")
    defer { cleanup() }
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(
        r["error"].asString?.hasPrefix(
            "static_loader: create failed for {'id': 'a', 'handler_module': 5}: ") == true,
        "\(r.serialize())")
}

/// A8.22 — a manifest mapping keeps its DOCUMENT order, so every `{spec!r}`
/// text reads as python prints it. (The persisted record's meta order is
/// asserted with C15.)
@Test func manifestMappingsKeepDocumentOrder() async {
    let (kernel, cleanup) = await bootLoader("agents:\n  - zeta: 1\n    alpha: 2\n    id: a\n")
    defer { cleanup() }
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(
        r["error"].asString
            == "static_loader: each roster entry needs a handler_module: "
            + "{'zeta': 1, 'alpha': 2, 'id': 'a'}", "\(r.serialize())")
}

/// A8.14 — a spec `type` key OVERRIDES the create verb (python's spread order),
/// so the roster entry is not created.
@Test func aSpecTypeKeyOverridesTheCreateVerb() async {
    let (kernel, cleanup) = await bootLoader(
        "agents:\n  - id: a\n    handler_module: cli.tools\n    type: reflect\n")
    defer { cleanup() }
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    // python spreads the spec AFTER `type`, so the roster entry becomes a
    // `reflect` of the root: no agent is created, and the reply's own `id` is
    // what lands in `created` — the same reply python's `rec.get("id")` reads.
    #expect(kernel.agent(AgentId("a")) == nil, "the create verb was overridden: \(r.serialize())")
    #expect(r["booted"].asBool == true, "\(r.serialize())")
    #expect(
        (r["created"].asArray ?? []).compactMap { $0.asString } == ["core"], "\(r.serialize())")
}

/// A8.6 — the handler_module mismatch text is python's, with `{!r}` on all
/// three values and no `Some(...)`.
@Test func aManifestThatReassignsAHandlerModuleFailsLoud() async {
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
    let (path, cleanup2) = writeManifest(
        "agents:\n  - id: memory\n    handler_module: text_file.tools\n")
    defer { cleanup2() }
    _ = await kernel.send([
        Item(
            "core",
            [
                "type": "update_agent", "id": "static_loader", "manifest": .string(path),
            ])
    ])[0].result
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(
        r["error"].asString
            == "static_loader: 'memory' exists as 'cli.tools' but the manifest declares "
            + "'text_file.tools' — a handler_module is fixed at create; delete the agent "
            + "to re-declare it", "\(r.serialize())")
    #expect(r["booted"].asBool == false, "\(r.serialize())")
}

/// A8.11 — reflect's `display_name` falls back to the id when the stored name
/// is empty (python `agent.display_name or id`).
@Test func reflectDisplayNameEmptyFallsBackToId() async {
    let (kernel, cleanup) = await bootLoader(MANIFEST)
    defer { cleanup() }
    _ = await kernel.send([
        Item(
            "core",
            [
                "type": "update_agent", "id": "static_loader", "display_name": "",
            ])
    ])[0].result
    let r = await kernel.send([Item("static_loader", ["type": "reflect"])])[0].result
    #expect(r["display_name"].asString == "static_loader", "\(r.serialize())")
    #expect(
        r["loader"].asBool == true && r["sentence"].asString == "declarative YAML roster loader")
}

/// A8.13 — a YAML parse error is reported under python's prefix. (D4 residual:
/// the tail is the YAML library's own message; PyYAML's `MarkedYAMLError`
/// wording cannot be reproduced from libyaml.)
@Test func yamlParseErrorIsPyYAMLText() async {
    let (kernel, cleanup) = await bootLoader("a: [1\n")
    defer { cleanup() }
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(r["booted"].asBool == false, "\(r.serialize())")
    #expect(
        r["error"].asString?.contains("is not valid YAML: ") == true, "\(r.serialize())")
}

/// A8.23 — the bundle readme is python's `readme.md`, byte for byte.
@Test func readmeIsPythonVerbatim() throws {
    let expected = try String(
        contentsOf: pythonSource("loader/static_loader/src/static_loader/readme.md"),
        encoding: .utf8)
    #expect(StaticLoaderBundle().readme == expected)
}

@Test func aRosterEntryWithoutAnIdIsRefusedLoud() async {
    // #105 — the roster is keyed by id, so an id-less entry can never match the
    // agent the LAST boot minted: every reboot created ANOTHER one beside it and
    // the flush persisted them all, with no word to the operator.
    let (kernel, cleanup) = await bootLoader(
        """
        agents:
          - handler_module: cli.tools
        """)
    defer { cleanup() }
    let before = kernel.allAgents().count
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(r["booted"].asBool == false, "\(r.serialize())")
    let err = r["error"].asString ?? ""
    #expect(err.contains("explicit id"), "\(err)")
    #expect(err.contains("cli.tools"), "the entry is named: \(err)")
    // NOTHING was created — not on this boot, and not by the create-time boot.
    #expect(kernel.allAgents().count == before)
    #expect(before == 2, "just the root + the loader itself")
}

@Test func anIdLessEntryIsRefusedBeforeTheValidOnesHydrate() async {
    // The refusal is a REFUSAL, not a skip: boot stops at the bad entry.
    let (kernel, cleanup) = await bootLoader(
        """
        agents:
          - handler_module: cli.tools
          - id: memory
            handler_module: cli.tools
        """)
    defer { cleanup() }
    let r = await kernel.send([Item("static_loader", ["type": "boot"])])[0].result
    #expect(r["booted"].asBool == false, "\(r.serialize())")
    #expect(kernel.agent(AgentId("memory")) == nil)
}
