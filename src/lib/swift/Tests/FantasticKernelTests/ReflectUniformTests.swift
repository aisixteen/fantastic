// Unit tests for the uniform `reflect` surface — tree/bundles/readme
// flags, the `description` field, and the guarantee that the old
// root-only primer keys are gone. Mirrors Python's test_reflect_flags
// and Rust's reflect/tests.rs.

import FantasticJSON
import Foundation
import Testing

@testable import FantasticKernel

@Suite("Uniform reflect")
struct ReflectUniformTests {
    private static let primerKeysGone = [
        "transports", "primitive", "envelope", "universal_verb",
        "binary_protocol", "browser_bus", "well_known", "agent_count",
        "available_bundles",
    ]

    @Test func rootReflectIsUniformNoPrimerKeys() async {
        let kernel = makeKernel()
        let r = await kernel.send([Item("core", ["type": "reflect"])])[0].result
        #expect(r["id"].asString == "core")
        #expect(r["sentence"].asString?.hasPrefix("Fantastic kernel") == true)
        #expect(r["parent_id"].isNull)
        // tree default = all.
        #expect(r["tree"]["id"].asString == "core")
        // bundles omitted by default.
        #expect(r["bundles"].asArray == nil)
        // kernel runtime identity — root only, lowercase enum.
        #expect(r["runtime"].asString == "swift")
        for k in Self.primerKeysGone {
            #expect(r[k] == nil || r[k] == .null, "primer key \(k) still present")
        }
    }

    @Test func rootReflectSurfacesGuidFromMeta() async {
        let kernel = makeKernel()
        // Bare root has no guid in meta → reflect surfaces an explicit null
        // (persistence-scoped field, always present on the root reflect).
        let bare = await kernel.send([Item("core", ["type": "reflect"])])[0].result
        #expect(bare["guid"].isNull)
        // Seed a 32-hex guid into root meta → reflect surfaces it verbatim.
        kernel.root?.updateMeta(["guid": .string("0123456789abcdef0123456789abcdef")])
        let r = await kernel.send([Item("core", ["type": "reflect"])])[0].result
        #expect(r["guid"].asString == "0123456789abcdef0123456789abcdef")
        // Non-root reflects never carry guid.
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "echo.tools", "id": "kid"])])[0].result
        let kid = await kernel.send([Item("kid", ["type": "reflect"])])[0].result
        #expect(kid["guid"] == nil || kid["guid"].isNull)
    }

    @Test func treeTiers() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "echo.tools", "id": "kid"])])[0].result
        // tree=ids → flat list, root first.
        let ids = await kernel.send([Item("core", ["type": "reflect", "tree": "ids"])])[0].result
        #expect(ids["tree"].asArray?.compactMap { $0.asString } == ["core", "kid"])
        // tree=none → omitted.
        let none = await kernel.send([Item("core", ["type": "reflect", "tree": "none"])])[0].result
        #expect(none["tree"] == nil || none["tree"].isNull)
    }

    @Test func bundlesTiers() async {
        let kernel = makeKernel()
        let all = await kernel.send([Item("core", ["type": "reflect", "bundles": "all"])])[0].result
        let arr = all["bundles"].asArray
        #expect(arr != nil && !(arr!.isEmpty))
        #expect(arr?.first?["name"].asString != nil)
        #expect(arr?.first?["handler_module"].asString != nil)
        let ids = await kernel.send([Item("core", ["type": "reflect", "bundles": "ids"])])[0].result
        #expect(ids["bundles"].asArray?.allSatisfy { $0.asString != nil } == true)
    }

    @Test func descriptionSurfacesTopAndTree() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("core", [
                "type": "create_agent", "parent_id": "core", "handler_module": "echo.tools",
                "id": "kid", "description": "holds my notes",
            ])])[0].result
        // top-level on the child's own reflect.
        let own = await kernel.send([Item("kid", ["type": "reflect"])])[0].result
        #expect(own["description"].asString == "holds my notes")
        // and in the parent's tree=all node.
        let root = await kernel.send([Item("core", ["type": "reflect"])])[0].result
        let node = root["tree"]["children"].asArray?.first { $0["id"].asString == "kid" }
        #expect(node?["description"].asString == "holds my notes")
    }

    @Test func readmeFlagTiersAndFallback() async {
        let kernel = makeKernel()
        // default → no readme key.
        let plain = await kernel.send([Item("core", ["type": "reflect"])])[0].result
        #expect(plain["readme"] == nil || plain["readme"].isNull)
        // readme=true on the root in-memory → embedded canonical readme.
        let r = await kernel.send([Item("core", ["type": "reflect", "readme": true])])[0].result
        #expect(r["readme"].asString == RootReadme.text)
        #expect(r["readme"].asString?.hasPrefix("# This is a Fantastic kernel.") == true)
        // readme honored.
        let legacy = await kernel.send([Item("core", ["type": "reflect", "readme": true])])[0].result
        #expect(legacy["readme"].asString == RootReadme.text)
    }

    @Test func embeddedReadmeIsNonEmpty() {
        // The bundled resource must load (Bundle.module wired correctly).
        #expect(!RootReadme.text.isEmpty)
        #expect(RootReadme.text.contains("reflect"))
    }

    // ─── keyed / tiered doc tree ───────────────────────────────

    @Test func embeddedDocsLoad() {
        // The `docs/` resource tree must load (Bundle.module wired), each with a
        // `> ` descriptor line the index reads.
        #expect(RootDocs.all.count == RootDocs.keys.count)
        #expect(RootDocs.all.contains { $0.key == "self-extension" })
        for (key, body) in RootDocs.all {
            #expect(!body.isEmpty, "empty doc \(key)")
            #expect(body.hasPrefix("> "), "doc \(key) must open with a `> ` descriptor")
        }
    }

    /// The authoring doc must teach the FILE+COMMAND execution model — a durable
    /// program is a FILE you write (the stream verbs, the document verbatim in
    /// `<bytes>`) plus a command you run on the general-purpose exec_bridge
    /// COMMAND runner. No `code` field.
    @Test func selfExtensionDocStatesTheFileCommandModel() throws {
        let body = try #require(RootDocs.all.first { $0.key == "self-extension" }?.body)
        #expect(body.contains("write_stream"), "\(body)")
        #expect(body.contains("<bytes>"), "\(body)")
        // `send_binary` is GONE: there is no such tool in the call grammar, and
        // the byte path is the verbatim `<bytes>` value.
        #expect(!body.contains("send_binary"), "\(body)")
        #expect(body.lowercased().contains("command runner"), "\(body)")
        #expect(body.contains("`code` field"), "\(body)")
        // The superseded script model must be gone, not merely supplemented.
        #expect(!body.contains("IS the tool"), "\(body)")
    }

    @Test func rootReflectCarriesReadmesFrame() async {
        let kernel = makeKernel()
        // `detail:'full'` carries the doc-tree FRAME (readmes + docs); the distilled
        // DEFAULT omits it — the always-injected AI/FM prompt stays in budget.
        // Mirrors python `test_readme_doctree.py` (full carries, default omits).
        let r = await kernel.send([Item("core", ["type": "reflect", "detail": "full"])])[0].result
        let readmes = r["readmes"].asArray
        #expect(readmes != nil && !(readmes!.isEmpty))
        #expect(readmes?.contains { $0["key"].asString == "self-extension" } == true)
        for e in readmes ?? [] {
            #expect(e["desc"].asString?.isEmpty == false)
            #expect((e["tokens"].asInt ?? 0) > 0)
            #expect(e["body"] == nil || e["body"].isNull)  // index carries no bodies
        }
        #expect(r["docs"].asString?.contains("keyed doc tree") == true)

        // The distilled default drops the inlined frame (readmes + docs).
        let d = await kernel.send([Item("core", ["type": "reflect"])])[0].result
        #expect(d["readmes"] == nil || d["readmes"].isNull)
        #expect(d["docs"] == nil || d["docs"].isNull)
    }

    @Test func readmeIndexKeyedViewAndErrors() async {
        let kernel = makeKernel()
        // readme=index → the token-annotated topic tree.
        let idx = await kernel.send([Item("core", ["type": "reflect", "readme": "index"])])[0].result
        #expect(
            idx["readme"]["index"].asArray?.contains { $0["key"].asString == "self-extension" }
                == true)
        // readme=<key> → the topic body + tokens.
        let doc = await kernel.send([Item("core", ["type": "reflect", "readme": "self-extension"])])[0].result
        #expect(doc["readme"]["key"].asString == "self-extension")
        #expect(doc["readme"]["body"].asString?.lowercased().contains("author") == true)
        #expect((doc["readme"]["tokens"].asInt ?? 0) > 0)
        // unknown key → {error, keys}.
        let miss = await kernel.send([Item("core", ["type": "reflect", "readme": "nope"])])[0].result
        #expect(miss["readme"]["error"].asString != nil)
        #expect(
            miss["readme"]["keys"].asArray?.contains { $0.asString == "self-extension" } == true)
        // a `..` escape is refused.
        let esc = await kernel.send([Item("core", ["type": "reflect", "readme": "../root_readme"])])[0].result
        #expect(esc["readme"]["error"].asString != nil)
    }

    @Test func nonRootHasNoReadmesFrame() async {
        let kernel = makeKernel()
        _ = await kernel.send([Item("core", ["type": "create_agent", "parent_id": "core", "handler_module": "echo.tools", "id": "kid"])])[0].result
        let kid = await kernel.send([Item("kid", ["type": "reflect"])])[0].result
        #expect(kid["readmes"] == nil || kid["readmes"].isNull)
        #expect(kid["docs"] == nil || kid["docs"].isNull)
    }

    // ─── self_extension: the authoring contract ────────────────
    //
    // A durable program is a FILE plus a COMMAND — there is no `code` field
    // (a program embedded as a JSON string is exactly how a call gets lost to
    // escaping). Write it with the file leg's STREAM verbs, the document
    // verbatim in `<bytes>`, and run it on the exec_bridge COMMAND runner.

    @Test func selfExtensionStatesTheFileCommandModel() async throws {
        let kernel = makeKernel()
        let s = try #require(
            await kernel.send([Item("core", ["type": "reflect"])])[0].result["self_extension"].asString)
        #expect(s.contains("A DURABLE PROGRAM is a FILE"), "\(s)")
        #expect(s.contains("no `code` field") || s.contains("There is no `code` field"), "\(s)")
        #expect(s.contains("write_stream"), "\(s)")
        #expect(s.contains("<bytes>"), "\(s)")
        #expect(!s.contains("send_binary"), "\(s)")
        #expect(!s.contains("Begin Bytes"), "\(s)")
        #expect(s.contains("COMMAND runner"), "\(s)")
        #expect(s.contains("exec_bridge.tools"), "\(s)")
        // The superseded script phrasing must be gone.
        #expect(!s.contains("IS the tool"), "\(s)")
        #expect(!s.contains("code:'<your program"), "\(s)")
    }
}
