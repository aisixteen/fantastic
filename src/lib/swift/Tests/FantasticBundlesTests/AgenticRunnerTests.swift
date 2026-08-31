// THE AGENTIC RUNNER — swift's implementation over the SHARED corpus.
//
// Reads `contract/agentic/{configs,specs}` at the repo root: the same files
// python's runner reads (`src/lib/python/tests/agentic/_engine.py`) and rust's
// (`crates/bundles/fantastic-static-loader/tests/agentic.rs`). A scenario is
// DATA; the runner is the only per-runtime part.
//
// `$root` in a config or spec means THIS runtime's root — `core` here,
// `kernel_state` in python. Parity asserts the per-runtime value, never
// equality.

import FantasticExecBridge
import FantasticFile
import FantasticJSON
import FantasticKernel
import FantasticStaticLoader
import FantasticDurableMemory
import Foundation
import OrderedCollections
import Testing
import Yams

/// THIS runtime's root id.
private let ROOT = "core"

private func contractDir() -> URL {
    // From this file: Tests/FantasticBundlesTests/ → swift → lib → src → repo
    URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // FantasticBundlesTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // swift
        .deletingLastPathComponent()  // lib
        .deletingLastPathComponent()  // src
        .deletingLastPathComponent()  // repo root
        .appendingPathComponent("contract")
}

// ─── corpus loading ─────────────────────────────────────────────

/// `$root` anywhere — key or value — becomes this runtime's root id.
private func substRoot(_ v: JSON) -> JSON {
    switch v {
    case .string(let s) where s == "$root": return .string(ROOT)
    case .array(let a): return .array(a.map(substRoot))
    case .object(let o):
        var out = OrderedDictionary<String, JSON>()
        for (k, x) in o { out[k == "$root" ? ROOT : k] = substRoot(x) }
        return .object(out)
    default: return v
    }
}

/// Yams' `Any` tree → our `JSON`, walked by hand.
///
/// `internal`, not `private`: the SCENARIO runner in this same target reads the
/// shared scenarios and needs exactly this walk. One copy — a second would drift
/// on the case that motivated it (a bare null, which `JSONSerialization`
/// refuses and the corpus relies on).
///
/// Not via `JSONSerialization`: a YAML document carries values it will not
/// represent (a bare null, a non-string key, a top-level scalar), and the corpus
/// is full of `side: null` — the matcher that tells an io seal from the call
/// gate. Going through JSONSerialization threw on exactly that.
func anyToJSON(_ v: Any?) -> JSON {
    switch v {
    case nil, is NSNull: return .null
    case let b as Bool: return .bool(b)
    case let i as Int: return .integer(Int64(i))
    case let d as Double: return .double(d)
    case let s as String: return .string(s)
    case let a as [Any?]: return .array(a.map(anyToJSON))
    case let m as [String: Any?]:
        var out = OrderedDictionary<String, JSON>()
        for k in m.keys.sorted() { out[k] = anyToJSON(m[k] ?? nil) }
        return .object(out)
    case let m as [AnyHashable: Any?]:
        var out = OrderedDictionary<String, JSON>()
        for (k, x) in m { out["\(k)"] = anyToJSON(x) }
        return .object(out)
    default: return .string("\(v!)")
    }
}

private func yamlToJSON(_ path: URL) throws -> JSON {
    let text = try String(contentsOf: path, encoding: .utf8)
    return substRoot(anyToJSON(try Yams.load(yaml: text)))
}

private struct Scenario {
    let name: String
    let spec: JSON
    let config: JSON
}

private func loadScenarios() throws -> [Scenario] {
    let specsDir = contractDir().appendingPathComponent("agentic/specs")
    let paths = try FileManager.default
        .contentsOfDirectory(at: specsDir, includingPropertiesForKeys: nil)
        .filter { $0.pathExtension == "yaml" }
        .sorted { $0.lastPathComponent < $1.lastPathComponent }
    var out: [Scenario] = []
    for p in paths {
        let spec = try yamlToJSON(p)
        let cfgName = spec["config"].asString ?? ""
        let config = try yamlToJSON(
            contractDir().appendingPathComponent("agentic/configs/\(cfgName)"))
        out.append(
            Scenario(
                name: p.deletingPathExtension().lastPathComponent, spec: spec, config: config))
    }
    return out
}

// ─── the probe the cancel scenarios need ────────────────────────

/// A verb that stays IN FLIGHT until cancelled. No shipped bundle offers one
/// without a subprocess or a socket, and inventing a kernel mechanism to make a
/// test expressible is not on — so each runtime ships this same small probe.
private struct LifecycleProbe: AgentBundle {
    let name = "lifecycle_probe"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "boot", "shutdown": return nil
        case "reflect":
            return .object([
                "id": .string(agentId.value),
                "sentence": .string("a probe agent whose `slow` verb stays in flight"),
                "verbs": .object(["slow": .string("no args. Blocks until cancelled.")]),
            ])
        case "slow":
            try await Task.sleep(nanoseconds: 30_000_000_000)
            return .object(["finished": .bool(true)])
        case let other:
            return .object([
                "error": .string("lifecycle_probe: unknown type \"\(other)\""),
                "reason": .string("unhandled"), "verb": .string(other),
            ])
        }
    }
}

// ─── hydration ──────────────────────────────────────────────────

private func makeKernel(workdir: URL) -> Kernel {
    let registry = BundleRegistry()
    registry.register("static_loader.tools", StaticLoaderBundle())
    registry.register("file_bridge.tools", FileBundle())
    registry.register("durable_memory.tools", DurableMemoryBundle())
    registry.register("exec_bridge.tools", ExecBridgeBundle())
    registry.register("lifecycle_probe.tools", LifecycleProbe())
    let kernel = Kernel(storage: .disk(workdir), bundles: registry)
    // The root's rootPath IS the store root the configs declare (`.fantastic`).
    // `findStore` matches a candidate provider's `root` meta against THIS path,
    // so a bare workdir here would make every declared store undiscoverable and
    // the whole tree would silently stay in RAM.
    let root = Agent(
        id: AgentId(ROOT), handlerModule: nil, parentId: nil,
        rootPath: workdir.appendingPathComponent(".fantastic"))
    kernel.register(root)
    kernel.setRoot(root)
    return kernel
}

private struct Hydration {
    var kernel: Kernel
    let workdir: URL
    /// The manifest fixture under the CWD — removed by the caller.
    var fixture: URL = URL(fileURLWithPath: "/dev/null")
    var creates: [String: JSON]
    /// The EXPLICIT boot reply per loader. `create_agent` boots its child and
    /// SWALLOWS the exception, so a manifest that parses but cannot build is
    /// invisible on the create — an explicit boot is the only loud channel.
    var boots: [String: JSON]
}

private func hydrate(_ sc: Scenario, workdir: URL) async throws -> Hydration {
    let kernel = makeKernel(workdir: workdir)
    // Under the CWD, because the manifest read is clamped to the running dir.
    // A unique name per scenario keeps parallel suites from colliding, and the
    // caller removes it — a leaked fixture is a directory that reappears in
    // `git status` on every run.
    let fixtureDirName = ".agentic_\(UUID().uuidString.prefix(8))"
    let fixture = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
        .appendingPathComponent(fixtureDirName)
    try FileManager.default.createDirectory(at: fixture, withIntermediateDirectories: true)
    var creates: [String: JSON] = [:]
    var boots: [String: JSON] = [:]

    for loader in sc.config["loaders"].asArray ?? [] {
        let lid = loader["id"].asString ?? "loader"
        let file = loader["file"].asString ?? "fantastic.yaml"
        let path = fixture.appendingPathComponent(file)
        if case .object = loader["manifest"] {
            // Written as JSON, deliberately: YAML is a SUPERSET of JSON, so the
            // loader reads this unchanged, and round-tripping through a YAML
            // emitter would have to represent values it refuses (a bare null).
            try loader["manifest"].serialize().write(to: path, atomically: true, encoding: .utf8)
        } else if let text = loader["manifest_text"].asString {
            try text.write(to: path, atomically: true, encoding: .utf8)
        }
        // `missing_manifest: true` writes NOTHING — the loader points at a path
        // that is not there, and refusing that is the assertion.
        //
        // RELATIVE, and the manifest lives UNDER THE RUNNING DIR — the same
        // shape python and rust use. `static_loader` reads through the
        // running-dir clamp, so a manifest in some unrelated temp dir is
        // refused, which is the point of the clamp. The runner cannot chdir
        // (sibling suites share this process), so the file comes to the cwd
        // instead of the cwd going to it — exactly what rust's loader tests do.
        let rel = "\(fixtureDirName)/\(file)"

        var payload = OrderedDictionary<String, JSON>()
        payload["type"] = .string("create_agent")
        payload["parent_id"] = .string("kernel")
        payload["handler_module"] = .string("static_loader.tools")
        payload["id"] = .string(lid)
        payload["manifest"] = .string(rel)
        if case .object(let meta) = loader["meta"] {
            for (k, v) in meta { payload[k] = v }
        }
        let rec = await kernel.send([Item(AgentId(ROOT), .object(payload))])[0].result
        let built = rec["error"] == .null
        creates[lid] = rec
        if built {
            boots[lid] = await kernel.send([Item(AgentId(lid), .object(["type": .string("boot")]))])[0].result
        }
    }
    return Hydration(
        kernel: kernel, workdir: workdir, fixture: fixture, creates: creates, boots: boots)
}

// ─── the primitives ─────────────────────────────────────────────

/// Send AS an agent — GATED. The only evidence about reach: outside a dispatch
/// there is no sender, which makes a bare send an OPERATOR send and therefore
/// worthless as evidence about an agent's authority.
private func reach(_ k: Kernel, _ sender: String, _ target: String, _ payload: JSON) async -> JSON {
    await dispatchCorpus(k, sender: AgentId(sender), target: target, payload: payload)
}

private func operatorSend(_ k: Kernel, _ target: String, _ payload: JSON) async -> JSON {
    await dispatchCorpus(k, sender: nil, target: target, payload: payload)
}

/// Dispatch one corpus send under `sender` (nil = operator, ungated). A
/// `write_stream`/`read_stream` rides the BINARY channel — file_bridge's string
/// verbs are gone. Mirrors python `_engine.py`: for a write, `bytes_utf8` becomes
/// the raw blob; for a read, the returned blob folds back into the reply's
/// `bytes` field (utf-8) so a `contains: {bytes: ...}` matcher can see it. The
/// gate still applies — `sendWithBinary` honours the two-sided call gate when a
/// sender is set.
private func dispatchCorpus(_ k: Kernel, sender: AgentId?, target: String, payload: JSON) async
    -> JSON
{
    await KernelTaskLocals.$currentSender.withValue(sender) {
        switch payload["type"].asString ?? "" {
        case "write_stream":
            var header = payload
            if case .object(var o) = header {
                o["bytes_utf8"] = nil
                header = .object(o)
            }
            let blob = Data((payload["bytes_utf8"].asString ?? "").utf8)
            return await k.sendWithBinary(AgentId(target), header, blob).0
        case "read_stream":
            let (reply, body) = await k.sendWithBinary(AgentId(target), payload, Data())
            if case .object(var o) = reply {
                o["bytes"] = .string(String(data: body, encoding: .utf8) ?? "")
                return .object(o)
            }
            return reply
        default:
            return await k.send([Item(AgentId(target), payload)])[0].result
        }
    }
}

/// The ONE machine-readable question about a reply. Never "is there an error":
/// a bundle error and a gate denial are different animals, and only the gate
/// sets `reason: unauthorized`.
private func denied(_ r: JSON) -> Bool { r["reason"].asString == "unauthorized" }
private func denialSide(_ r: JSON) -> String? { r["side"].asString }

/// Resolve a dotted path, `*` meaning "every element of this list".
private func dig(_ v: JSON, _ path: String) -> [JSON] {
    var cur = [v]
    for part in path.split(separator: ".") {
        var next: [JSON] = []
        for c in cur {
            if part == "*" {
                if let a = c.asArray { next.append(contentsOf: a) }
            } else if c[String(part)] != .null {
                next.append(c[String(part)])
            }
        }
        cur = next
    }
    return cur
}

/// Substitute `$name.field` references bound by earlier steps — a `start` binds
/// its task, so a later `cancel` can name the pid the KERNEL minted.
private func resolve(_ v: JSON, _ bound: [String: JSON]) -> JSON {
    switch v {
    case .string(let s) where s.hasPrefix("$"):
        let ref = String(s.dropFirst())
        let parts = ref.split(separator: ".", maxSplits: 1)
        guard let target = bound[String(parts[0])] else { return v }
        if parts.count == 1 { return target }
        return dig(target, String(parts[1])).first ?? .null
    case .array(let a): return .array(a.map { resolve($0, bound) })
    case .object(let o):
        var out = OrderedDictionary<String, JSON>()
        for (k, x) in o { out[k] = resolve(x, bound) }
        return .object(out)
    default: return v
    }
}

private func matchStep(_ label: String, _ expect: JSON, _ reply: JSON) {
    func fail(_ why: String) { Issue.record("\(label): \(why)\n  reply: \(reply.serialize())") }

    if let allow = expect["allow"].asBool {
        if allow && denied(reply) { fail("expected the edge to be OPEN, it was denied") }
        if !allow && !denied(reply) { fail("expected a DENIAL, the call went through") }
    }
    if expect.asObject?["side"] != nil {
        // `side: null` — the refusal must carry NO side. That is what tells an
        // io SEAL from the call gate: both answer `unauthorized`, but only the
        // gate names a half, because only the gate is about the caller.
        if expect["side"] == .null {
            if let got = denialSide(reply) { fail("expected NO side (an io seal), got \"\(got)\"") }
        } else if denialSide(reply) != expect["side"].asString {
            fail("expected the \(expect["side"].asString ?? "?") half to refuse, got \(denialSide(reply) ?? "none")")
        }
    }
    if let want = expect["reason"].asString, reply["reason"].asString != want {
        fail("expected reason \"\(want)\"")
    }
    if let want = expect["status"].asString, reply["status"].asString != want {
        fail("expected status \"\(want)\"")
    }
    if let sub = expect["error_contains"].asString,
        !(reply["error"].asString ?? "").contains(sub)
    {
        fail("error does not contain \"\(sub)\"")
    }
    if case .object(let m) = expect["contains"] {
        for (path, want) in m where !dig(reply, path).contains(where: { pyEq($0, want) }) {
            fail("\(path) does not contain \(want.serialize())")
        }
    }
    if case .object(let m) = expect["not_contains"] {
        for (path, unwanted) in m where dig(reply, path).contains(where: { pyEq($0, unwanted) }) {
            fail("\(path) contains \(unwanted.serialize()) and must not")
        }
    }
}

/// Python-flavoured equality for the corpus matcher: exact JSON equality, PLUS
/// the `True == 1` / `False == 0` the corpus authors rely on (`contains:
/// {written: true}` against a reply that may carry `1`, and the reverse). Rust
/// needed the same `py_eq` helper.
private func pyEq(_ a: JSON, _ b: JSON) -> Bool {
    if a == b { return true }
    func num(_ j: JSON) -> Double? {
        switch j {
        case .bool(let x): return x ? 1 : 0
        case .integer(let i): return Double(i)
        case .double(let d): return d
        default: return nil
        }
    }
    if let x = num(a), let y = num(b) { return x == y }
    return false
}

/// `grant`/`revoke` — sugar over an OPERATOR `update_agent`.
///
/// Sugar, not a shortcut: `update_agent` REPLACES a field rather than merging it
/// (#6), so a spec that hand-wrote the payload would restate the whole map every
/// time and would silently drop the structural baseline off the live record.
private func grantStep(_ h: Hydration, _ args: JSON, remove: Bool) async -> JSON {
    let agentId = args["agent"].asString ?? ""
    let field = args["field"].asString ?? ""
    let other = args["to"].asString ?? ""
    guard let agent = h.kernel.agent(AgentId(agentId)) else {
        Issue.record("grant: no agent \"\(agentId)\"")
        return .null
    }
    var current = agent.metaValue(forKey: field)?.asObject ?? OrderedDictionary<String, JSON>()
    if remove { current.removeValue(forKey: other) } else {
        current[other] = args["verbs"] != .null ? args["verbs"] : .array([.string("*")])
    }
    return await operatorSend(
        h.kernel, "kernel",
        .object(["type": .string("update_agent"), "id": .string(agentId), field: .object(current)]))
}

// ─── the step executor ──────────────────────────────────────────

private func manifestSpecs(_ config: JSON) -> [(String, JSON)] {
    var out: [(String, JSON)] = []
    for l in config["loaders"].asArray ?? [] {
        for a in l["manifest"]["agents"].asArray ?? [] {
            if let id = a["id"].asString { out.append((id, a)) }
        }
    }
    return out
}

/// The verbs `spec` declares for `other`. A `"*"` is REFUSED rather than
/// interpreted: a wildcard makes the per-verb intersection unobservable, and
/// that intersection is the whole point of the walk.
private func declaredVerbs(_ spec: JSON, _ field: String, _ other: String) -> Set<String> {
    let g = spec[field][other]
    if g == .null { return [] }
    guard let arr = g.asArray else {
        Issue.record("reconcile_grants: \(field)[\(other)] is not a verb list; a wildcard hides the intersection")
        return []
    }
    return Set(arr.compactMap { $0.asString })
}

private func payloadFor(_ args: JSON, _ verb: String) -> JSON {
    var p = OrderedDictionary<String, JSON>()
    p["type"] = .string(verb)
    if case .object(let extra) = args["payloads"][verb] {
        for (k, v) in extra { p[k] = v }
    }
    return .object(p)
}

private func dehydrate(_ k: Kernel) -> (ids: [String], children: [String: [String]]) {
    var ids: [String] = []
    var children: [String: [String]] = [:]
    var queue = [k.root!.id]
    while let id = queue.popLast() {
        guard let a = k.agent(id) else { continue }
        ids.append(id.value)
        let kids = a.children().map { $0.id.value }
        children[id.value] = kids
        queue.append(contentsOf: kids.map { AgentId($0) })
    }
    return (ids.sorted(), children)
}

private func runSteps(_ h0: Hydration, _ sc: Scenario, _ pinned: inout [String: Int]) async {
    var h = h0
    var bound: [String: JSON] = [:]
    var tasks: [String: Task<JSON, Never>] = [:]
    let steps = sc.spec["expect"]["steps"].asArray ?? []

    for (idx, step) in steps.enumerated() {
        let label = step["name"].asString ?? "<step>"
        let expect = resolve(step["expect"], bound)
        var reply: JSON = .null

        if case .object = step["reach"] {
            let a = resolve(step["reach"], bound)
            reply = await reach(h.kernel, a["from"].asString ?? "", a["to"].asString ?? "", a["send"])
        } else if case .object = step["operator"] {
            let a = resolve(step["operator"], bound)
            reply = await operatorSend(h.kernel, a["to"].asString ?? "", a["send"])
        } else if case .object = step["grant"] {
            reply = await grantStep(h, resolve(step["grant"], bound), remove: false)
        } else if case .object = step["revoke"] {
            reply = await grantStep(h, resolve(step["revoke"], bound), remove: true)
        } else if step.asObject?["reboot"] != nil {
            // PERSIST + REHYDRATE. The kernel is rebuilt from its own saved
            // state and each loader re-booted, so a step after this one asks
            // about what SURVIVED — not about what is still in RAM.
            let snapshot = h.kernel.save()
            let fresh = makeKernel(workdir: h.workdir)
            do { try fresh.load(json: snapshot) } catch {
                Issue.record("\(label): reload refused the snapshot: \(error)")
            }
            for lid in h.creates.keys {
                _ = await operatorSend(fresh, lid, .object(["type": .string("boot")]))
            }
            h = Hydration(
                kernel: fresh, workdir: h.workdir, fixture: h.fixture,
                creates: h.creates, boots: h.boots)
            reply = .object(["rebooted": .bool(true)])
        } else if step.asObject?["assert_tree"] != nil {
            let (ids, _) = dehydrate(h.kernel)
            reply = .object(["ids": .array(ids.map { .string($0) })])
        } else if case .object = step["reconcile_grants"] {
            let a = step["reconcile_grants"]
            let specs = manifestSpecs(sc.config)
            var probed = 0
            for (sender, sSpec) in specs {
                for (target, tSpec) in specs where sender != target {
                    let egress = declaredVerbs(sSpec, "can_call", target)
                    let ingress = declaredVerbs(tSpec, "callable_by", sender)
                    for verb in egress.union(ingress).sorted() {
                        probed += 1
                        let r = await reach(h.kernel, sender, target, payloadFor(a, verb))
                        let both = egress.contains(verb) && ingress.contains(verb)
                        if denied(r) == both {
                            Issue.record(
                                "\(label): \(sender) -\(verb)-> \(target): egress=\(egress.sorted()) ingress=\(ingress.sorted()) — expected \(both ? "reachable" : "refused"), got \(r.serialize())")
                        }
                        if both { continue }
                        let want = egress.contains(verb) ? "ingress" : "egress"
                        if denialSide(r) != want {
                            Issue.record("\(label): the half that omitted \"\(verb)\" is \(want), denial named \(denialSide(r) ?? "none")")
                        }
                    }
                }
            }
            let floor = a["min_probes"].asInt ?? 1
            #expect(probed >= floor, "\(label): only \(probed) declared triples (min \(floor))")
            reply = .object(["swept": .bool(true)])
        } else if case .object = step["sweep_undeclared"] {
            // The whole roster, every ordered pair. The reachable set must be
            // EXACTLY the structural baseline plus what the spec declared open
            // BEFORE this point — nothing rides along.
            let a = step["sweep_undeclared"]
            let (ids, children) = dehydrate(h.kernel)
            var structural = Set(ids.map { Pair($0, $0) })
            for (parent, kids) in children {
                for kid in kids where ids.contains(kid) && ids.contains(parent) {
                    structural.insert(Pair(parent, kid))
                    structural.insert(Pair(kid, parent))
                }
            }
            for verbJ in a["verbs"].asArray ?? [.string("reflect")] {
                let verb = verbJ.asString ?? "reflect"
                var declared = Set<Pair>()
                for prior in steps[..<idx] {
                    guard case .object = prior["reach"] else { continue }
                    if prior["expect"]["allow"].asBool == true,
                        prior["reach"]["send"]["type"].asString == verb
                    {
                        declared.insert(
                            Pair(prior["reach"]["from"].asString ?? "", prior["reach"]["to"].asString ?? ""))
                    }
                }
                let want = structural.union(declared)
                var got = Set<Pair>()
                for sender in ids {
                    for target in ids {
                        let r = await reach(h.kernel, sender, target, payloadFor(a, verb))
                        if denied(r) {
                            if !["egress", "ingress"].contains(denialSide(r) ?? "") {
                                Issue.record("\(label): verb \"\(verb)\": a refusal named no call-gate half — \(r.serialize()) (a sealed io leg denies with NO side, a DIFFERENT gate)")
                            }
                        } else {
                            got.insert(Pair(sender, target))
                        }
                    }
                }
                if got != want {
                    Issue.record("\(label): verb \"\(verb)\": undeclared \(got.subtracting(want).map(\.d).sorted()), declared-and-missing \(want.subtracting(got).map(\.d).sorted())")
                }
            }
            reply = .object(["swept": .bool(true)])
        } else if case .object = step["start"] {
            // Fire WITHOUT awaiting. The cancel rows need a dispatch that is
            // still running when the next step acts on it; nothing else in the
            // DSL can express "in flight".
            let a = step["start"]
            let k = h.kernel
            let (from, to, send) = (a["from"].asString ?? "", a["to"].asString ?? "", a["send"])
            tasks[a["name"].asString ?? ""] = Task {
                await KernelTaskLocals.$currentSender.withValue(AgentId(from)) {
                    let out = await k.send([Item(AgentId(to), send)])[0]
                    return JSON.object([
                        "pid": out.pid.map { JSON.string($0.value) } ?? .null,
                        "status": .string("\(out.status)"),
                        "result": out.result,
                    ])
                }
            }
            reply = .object(["started": .bool(true)])
        } else if case .object = step["await_task"] {
            // Block until the started send is visible in `ps`. A fixed sleep is
            // a race that passes on a fast machine and fails on a loaded one.
            let a = step["await_task"]
            let wantAgent = a["agent"].asString ?? ""
            var found: JSON? = nil
            for _ in 0..<400 {
                let rows = await operatorSend(h.kernel, "kernel", .object(["type": .string("ps")]))
                if let row = (rows["tasks"].asArray ?? []).first(where: { $0["agent"].asString == wantAgent }) {
                    found = row
                    break
                }
                try? await Task.sleep(nanoseconds: 5_000_000)
            }
            guard let row = found else {
                Issue.record("\(label): no task for agent \"\(wantAgent)\" appeared in `ps`")
                continue
            }
            bound[a["name"].asString ?? ""] = row
            reply = row
        } else if case .object = step["collect"] {
            let name = step["collect"]["name"].asString ?? ""
            guard let t = tasks.removeValue(forKey: name) else {
                Issue.record("\(label): nothing started under \"\(name)\"")
                continue
            }
            let outcome = await t.value
            // MERGE, never clobber: `await_task` bound the ps row (`id`) and
            // this binds the outcome (`pid`) — the SAME id, by the one-id-space
            // rule — so a later step can still name either.
            var merged = bound[name]?.asObject ?? OrderedDictionary<String, JSON>()
            for (k, v) in outcome.asObject ?? [:] { merged[k] = v }
            bound[name] = .object(merged)
            var r = outcome["result"].asObject ?? OrderedDictionary<String, JSON>()
            r["status"] = outcome["status"]
            reply = .object(r)
        } else {
            continue
        }

        matchStep(label, expect, reply)
        if let rid = step["because"].asString { pinned[rid, default: 0] += 1 }
    }
    for t in tasks.values { t.cancel() }
}

/// An ordered pair of ids, hashable for the reachability sweep.
private struct Pair: Hashable {
    let a: String
    let b: String
    init(_ a: String, _ b: String) {
        self.a = a
        self.b = b
    }
    var d: String { "\(a)->\(b)" }
}

// ─── the suite ──────────────────────────────────────────────────

@Suite("agentic corpus", .serialized)
struct AgenticCorpusTests {
    @Test func theSharedAgenticCorpusHoldsOnThisRuntime() async throws {
        let scenarios = try loadScenarios()
        #expect(!scenarios.isEmpty, "corpus loaded no scenarios")
        var pinned: [String: Int] = [:]
        var ran = 0

        for sc in scenarios {
            // THE WORKDIR IS THE RUNNING DIR, and it has to be.
            //
            // A config declares its store as `root: .fantastic`, which resolves
            // against the cwd, while the kernel's own root path comes from the
            // storage workdir — so the two only agree when workdir == cwd.
            // Point the kernel at a temp dir instead and `find_store` matches
            // nothing, or its writes escape the clamp. rust gets to chdir into
            // a temp dir because its runner owns the process; this one shares it
            // with sibling suites, so the work comes to the cwd instead.
            //
            // Everything the scenario writes is therefore cleaned up by hand:
            // snapshot the directory first and remove whatever is new. A
            // scenario that left `.fantastic` or `rooms/` behind would poison
            // the next one and show up in `git status` forever.
            let cwd = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            let before = Set(
                (try? FileManager.default.contentsOfDirectory(atPath: cwd.path)) ?? [])
            let workdir = cwd
            defer {
                let after = Set(
                    (try? FileManager.default.contentsOfDirectory(atPath: cwd.path)) ?? [])
                for stray in after.subtracting(before) {
                    try? FileManager.default.removeItem(at: cwd.appendingPathComponent(stray))
                }
            }

            let declared = sc.spec["expect"]["hydration"].asString ?? "ok"
            let h = try await hydrate(sc, workdir: workdir)
            defer { try? FileManager.default.removeItem(at: h.fixture) }

            // PC-5 reads BOTH loud channels: `create_agent`'s reply (a
            // write-time refusal — the loader never became an agent) and the
            // explicit boot's (a manifest that parses but cannot build).
            let loaders = sc.config["loaders"].asArray ?? []
            let allCreated = loaders.allSatisfy { h.creates[$0["id"].asString ?? ""]?["error"] == JSON.null }
            let allBooted = loaders.allSatisfy {
                let b = h.boots[$0["id"].asString ?? ""]
                return b != nil && b!["error"] == .null && b!["booted"] != .bool(false)
            }
            let built = allCreated && allBooted
            if declared == "ok" {
                #expect(built, "\(sc.name): declared hydration ok, creates=\(h.creates)")
                if !built { continue }
            } else {
                #expect(!built, "\(sc.name): declared hydration \(declared) but every loader built")
                continue  // nothing to reach in a system that was refused
            }
            await runSteps(h, sc, &pinned)
            ran += 1
        }

        print("swift agentic: \(ran)/\(scenarios.count) scenarios ran")
        print("swift agentic requirements pinned: "
            + pinned.sorted { $0.key < $1.key }.map { "\($0.key)x\($0.value)" }.joined(separator: ", "))
        #expect(ran > 0, "no scenario hydrated")
    }
}
