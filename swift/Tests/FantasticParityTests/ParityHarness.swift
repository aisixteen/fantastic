// Cross-runtime parity test harness — Python (canonical) ↔ Swift.
//
// Spawns the Python kernel CLI as a subprocess and fires identical
// verb payloads at both kernels. Diffs the JSON replies field-by-
// field; wire-format drift fails loudly here before it bites a real
// app consumer.
//
// Python is the canonical reference for the Fantastic protocol — when
// the two kernels disagree on wire shape, on-disk format, or verb
// payloads, the Swift kernel is wrong. This test is the mechanical
// drift detector that survives author memory.
//
// CI gating: the suite returns cleanly (no recorded failures) when
// `PYTHON_KERNEL_BIN` env var is unset or the binary isn't
// executable. To run locally:
//
//     cd python && uv sync
//     PYTHON_KERNEL_BIN="$(realpath ~/Projects/fantastic_canvas/python/.venv/bin/fantastic)" \
//         swift test --filter FantasticParityTests
//
// The previous Rust-targeted version of this harness lived at this
// path; Rust workspace retired with PR #20. Python takes the role
// because it's the canonical reference; the harness shape (subprocess
// + JSON diff) is preserved.

#if os(macOS)

    import FantasticJSON
    import FantasticKernel
    import FantasticKernelStartup
    import Foundation
    import Testing

    /// Locate the Python kernel's `fantastic` entry-point. Returns
    /// `nil` (causing tests to skip cleanly) when the env var is
    /// unset or the binary isn't executable on this host.
    private func pythonBinaryURL() -> URL? {
        let env = ProcessInfo.processInfo.environment
        guard let path = env["PYTHON_KERNEL_BIN"],
            FileManager.default.isExecutableFile(atPath: path)
        else {
            return nil
        }
        return URL(fileURLWithPath: path)
    }

    /// Run `fantastic <agentId> <verb> [k=v ...]` against the Python
    /// kernel's CLI. The Python CLI's one-shot RPC mode prints the
    /// JSON reply to stdout and exits.
    private func pythonOneShot(
        binary: URL,
        workdir: URL,
        agentId: String,
        verb: String,
        args: [String: String] = [:]
    ) throws -> JSON {
        let proc = Process()
        proc.executableURL = binary
        proc.currentDirectoryURL = workdir
        var argList = [agentId, verb]
        for (k, v) in args.sorted(by: { $0.key < $1.key }) {
            argList.append("\(k)=\(v)")
        }
        proc.arguments = argList
        let outPipe = Pipe()
        proc.standardOutput = outPipe
        proc.standardError = Pipe()
        try proc.run()
        proc.waitUntilExit()
        let data = outPipe.fileHandleForReading.readDataToEndOfFile()
        return try JSON.parse(data)
    }

    /// Run `fantastic reflect [<id>]` against the Python kernel's
    /// dedicated shorthand mode.
    private func pythonReflect(
        binary: URL, workdir: URL, agentId: String = "fs_loader",
        args: [String: String] = [:]
    ) throws -> JSON {
        // Python's root id is `fs_loader` (Swift/Rust use `core`) — the
        // root-id asymmetry is BY DESIGN, so the default targets python's
        // actual root rather than a hardcoded `core`.
        let proc = Process()
        proc.executableURL = binary
        proc.currentDirectoryURL = workdir
        var argList = ["reflect", agentId]
        for (k, v) in args.sorted(by: { $0.key < $1.key }) {
            argList.append("\(k)=\(v)")
        }
        proc.arguments = argList
        let pipe = Pipe()
        proc.standardOutput = pipe
        proc.standardError = Pipe()
        try proc.run()
        proc.waitUntilExit()
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        return try JSON.parse(data)
    }

    /// Compare two JSON values, returning `nil` on match or a
    /// human-readable string describing the first divergence. Object
    /// key insertion order IS checked because both runtimes preserve
    /// it (Python via `dict` order, Swift via `OrderedDictionary`).
    private func diffJSON(_ a: JSON, _ b: JSON, path: String = "$") -> String? {
        switch (a, b) {
        case (.null, .null), (.bool, .bool), (.integer, .integer),
            (.double, .double), (.string, .string):
            if a != b {
                return "\(path): \(a.serialize()) != \(b.serialize())"
            }
            return nil
        case (.array(let aArr), .array(let bArr)):
            if aArr.count != bArr.count {
                return "\(path): length \(aArr.count) != \(bArr.count)"
            }
            for i in aArr.indices {
                if let d = diffJSON(aArr[i], bArr[i], path: "\(path)[\(i)]") {
                    return d
                }
            }
            return nil
        case (.object(let aObj), .object(let bObj)):
            let aKeys = Array(aObj.keys)
            let bKeys = Array(bObj.keys)
            if aKeys != bKeys {
                return "\(path): keys \(aKeys) != \(bKeys)"
            }
            for k in aKeys {
                if let d = diffJSON(aObj[k] ?? .null, bObj[k] ?? .null, path: "\(path).\(k)") {
                    return d
                }
            }
            return nil
        default:
            return "\(path): type mismatch (\(a) vs \(b))"
        }
    }

    @Suite("Cross-runtime parity (Python ↔ Swift)", .serialized)
    struct CrossRuntimeParityTests {

        @Test func reflectOnRootHasSharedTopKeys() async throws {
            guard let binary = pythonBinaryURL() else {
                // Skip cleanly — no Issue.record. Empty suite passes
                // when PYTHON_KERNEL_BIN is unset.
                return
            }
            let tmp = makeTempDir()
            defer { try? FileManager.default.removeItem(at: tmp) }

            // Python side.
            let pyReply = try pythonReflect(binary: binary, workdir: tmp)

            // Swift side.
            let kernel = try await startKernelInMemory(portHint: 0)
            let swReply = await kernel.send(
                AgentId("core"), .object(["type": .string("reflect")]))

            // Both must answer with an `id` field. Root ids differ BY
            // DESIGN (python=fs_loader, swift=core), so assert each runtime's
            // own root id rather than cross-runtime equality.
            #expect(pyReply["id"].asString == "fs_loader", "python root id != fs_loader")
            #expect(swReply["id"].asString == "core", "swift root id != core")
        }

        @Test func listAgentsShape() async throws {
            guard let binary = pythonBinaryURL() else { return }
            let tmp = makeTempDir()
            defer { try? FileManager.default.removeItem(at: tmp) }

            // Python side.
            let pyReply = try pythonOneShot(
                binary: binary, workdir: tmp,
                agentId: "fs_loader", verb: "list_agents")

            // Swift side.
            let kernel = try await startKernelInMemory(portHint: 0)
            let swReply = await kernel.send(
                AgentId("core"), .object(["type": .string("list_agents")]))

            // Top-level shape: both must have an `agents` array.
            #expect(pyReply["agents"].asArray != nil, "python list_agents lacks 'agents'")
            #expect(swReply["agents"].asArray != nil, "swift list_agents lacks 'agents'")

            // Each agent entry must carry `id` and (if a non-root)
            // `handler_module`. Don't pin the agent set (initial
            // bundles differ between runtimes); pin only that every
            // entry has the required shape.
            for arr in [pyReply["agents"].asArray ?? [], swReply["agents"].asArray ?? []] {
                for entry in arr {
                    #expect(entry["id"].asString != nil, "agent entry missing 'id': \(entry.serialize())")
                }
            }
        }

        /// The new uniform reflect contract: both runtimes return
        /// `{id, sentence, tree, ...}` with NONE of the old primer keys,
        /// and `bundles` omitted by default.
        @Test func reflectContractParity() async throws {
            guard let binary = pythonBinaryURL() else { return }
            let tmp = makeTempDir()
            defer { try? FileManager.default.removeItem(at: tmp) }

            let pyReply = try pythonReflect(binary: binary, workdir: tmp)
            let kernel = try await startKernelInMemory(portHint: 0)
            let swReply = await kernel.send(
                AgentId("core"), .object(["type": .string("reflect")]))

            let gone = [
                "transports", "primitive", "envelope", "universal_verb",
                "binary_protocol", "browser_bus", "well_known",
                "agent_count", "available_bundles",
            ]
            for (label, r, root) in [("python", pyReply, "fs_loader"), ("swift", swReply, "core")] {
                #expect(r["id"].asString == root, "\(label) reflect id != \(root)")
                #expect(r["sentence"].asString != nil, "\(label) reflect lacks sentence")
                #expect(r["tree"]["id"].asString == root, "\(label) reflect tree.id != \(root)")
                #expect(r["bundles"].asArray == nil, "\(label) reflect has bundles by default")
                for k in gone {
                    #expect(r[k] == nil || r[k] == .null, "\(label) reflect still has primer key \(k)")
                }
            }
        }

        /// `reflect tree=ids` → a flat string-id list (root first) on
        /// both runtimes; `reflect bundles=all` → a `{name, handler_module}`
        /// array on both (don't pin the set — bundle lists differ).
        @Test func reflectTreeIdsAndBundlesParity() async throws {
            guard let binary = pythonBinaryURL() else { return }
            let tmp = makeTempDir()
            defer { try? FileManager.default.removeItem(at: tmp) }

            let pyIds = try pythonReflect(
                binary: binary, workdir: tmp, args: ["tree": "ids"])
            let kernel = try await startKernelInMemory(portHint: 0)
            let swIds = await kernel.send(
                AgentId("core"),
                .object(["type": .string("reflect"), "tree": .string("ids")]))
            for (label, r, root) in [("python", pyIds, "fs_loader"), ("swift", swIds, "core")] {
                let arr = r["tree"].asArray
                #expect(arr != nil, "\(label) tree=ids not an array")
                #expect(arr?.first?.asString == root, "\(label) tree=ids root not first")
            }

            let pyB = try pythonReflect(
                binary: binary, workdir: tmp, args: ["bundles": "all"])
            let swB = await kernel.send(
                AgentId("core"),
                .object(["type": .string("reflect"), "bundles": .string("all")]))
            for (label, r) in [("python", pyB), ("swift", swB)] {
                let arr = r["bundles"].asArray
                #expect(arr != nil && !(arr!.isEmpty), "\(label) bundles=all empty")
                #expect(
                    arr?.first?["name"].asString != nil
                        && arr?.first?["handler_module"].asString != nil,
                    "\(label) bundles=all entry missing name/handler_module")
            }
        }

        /// `reflect readme=true` returns the canonical root readme on
        /// BOTH runtimes — and Swift's embedded copy is byte-identical to
        /// Python's. This is the cross-runtime readme parity sentinel.
        @Test func rootReadmeByteIdenticalParity() async throws {
            guard let binary = pythonBinaryURL() else { return }
            let tmp = makeTempDir()
            defer { try? FileManager.default.removeItem(at: tmp) }

            let pyReply = try pythonReflect(
                binary: binary, workdir: tmp, args: ["readme": "true"])
            let pyReadme = pyReply["readme"].asString
            #expect(pyReadme != nil, "python reflect readme=true lacks readme")
            #expect(
                pyReadme?.hasPrefix("# This is a Fantastic kernel.") == true,
                "python readme not the canonical doc")

            // Swift's embedded readme must equal Python's, byte-for-byte.
            #expect(
                RootReadme.text == pyReadme,
                "Swift RootReadme.text diverged from Python's canonical readme")

            // And Swift's reflect readme=true returns the same bytes.
            let kernel = try await startKernelInMemory(portHint: 0)
            let swReply = await kernel.send(
                AgentId("core"),
                .object(["type": .string("reflect"), "readme": .bool(true)]))
            #expect(
                swReply["readme"].asString == pyReadme,
                "Swift reflect readme=true diverged from Python")
        }

        @Test func reflectErrorEnvelopeShape() async throws {
            guard let binary = pythonBinaryURL() else { return }
            let tmp = makeTempDir()
            defer { try? FileManager.default.removeItem(at: tmp) }

            // Both should return an `error` field for a non-existent
            // agent id. The wire shape is `{error: String, ...}`.
            let pyReply = try pythonReflect(
                binary: binary, workdir: tmp, agentId: "nonexistent_xyz")
            let kernel = try await startKernelInMemory(portHint: 0)
            let swReply = await kernel.send(
                AgentId("nonexistent_xyz"),
                .object(["type": .string("reflect")]))

            #expect(
                pyReply["error"].asString != nil,
                "python missing error on missing agent: \(pyReply.serialize())")
            #expect(
                swReply["error"].asString != nil,
                "swift missing error on missing agent: \(swReply.serialize())")
        }
    }

    private func makeTempDir() -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent(
            "fantastic-parity-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

#endif
