// Filesystem-as-agent bundle.
//
// Mirrors Rust's `fantastic-file::FileBundle`. Each agent owns a
// `root` directory (from agent meta) and a `readonly` flag; every
// verb resolves the supplied path within `root` and refuses if the
// canonical result escapes (`..` traversal protection).
//
// Verbs: reflect / boot / list / read / write / delete / rename / mkdir.

import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "file_bridge.tools"

/// Default hidden-file names hidden from `list`.
let DEFAULT_HIDDEN: Set<String> = [
    ".git", ".env", ".fantastic", "node_modules", "__pycache__",
]

let IMAGE_EXTENSIONS: Set<String> = [
    "png", "jpg", "jpeg", "gif", "webp", "svg",
]

public struct FileBundle: AgentBundle {
    public let name = "file_bridge"

    public init() {}

    public var readme: String? {
        "file_bridge — the gated filesystem edge of the io family. Each agent owns `root` (clamped"
            + " INSIDE the running dir — the running-dir law); verbs are path-safe."
    }

    public func handle(
        agentId: AgentId,
        payload: JSON,
        kernel: Kernel
    ) async throws -> JSON? {
        let verb = payload["type"].asString ?? ""
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent \(agentId)")])
        }
        // GATE — the fs edge is an io_bridge leg: SEALED by default. Every verb
        // except discovery/lifecycle is denied until opened (ingress_rule=allow_all).
        if let denied = gateVerb(
            ingressRule: agent.metaValue(forKey: "ingress_rule"),
            auth: agent.metaValue(forKey: "auth"),
            authToken: agent.metaValue(forKey: "auth_token")?.asString,
            agentId: agent.id.value,
            verb: verb)
        {
            return denied
        }
        switch verb {
        case "reflect":
            // Discovery shows the configured root verbatim (no clamp — py parity).
            return reflect(agent: agent)
        case "boot":
            return .object(["ok": .bool(true)])
        default:
            // Every DATA verb clamps the root to the running dir FIRST — the
            // running-dir law: a root that escapes the kernel's workdir refuses.
            guard let root = clampedRoot(agent: agent, kernel: kernel) else {
                return .object([
                    "error": .string("root escapes the running dir"),
                    "reason": .string("root_escapes_running_dir"),
                ])
            }
            switch verb {
            case "list":
                return listFiles(root: root, payload: payload)
            case "read":
                return readFile(root: root, payload: payload)
            case "write":
                return writeFile(root: root, agent: agent, payload: payload)
            case "delete":
                return deleteFile(root: root, agent: agent, payload: payload)
            case "rename":
                return renameFile(root: root, agent: agent, payload: payload)
            case "mkdir":
                return makeDir(root: root, agent: agent, payload: payload)
            case "pump":
                // The PUMP coordinates a SOURCE→SINK copy over the binary channel
                // (it never touches bytes itself), so it rides the text channel.
                return await pump(agent: agent, payload: payload, kernel: kernel)
            case "read_stream", "write_stream":
                // Stream verbs carry RAW BYTES — binary channel only.
                return .object([
                    "error": .string(
                        "\(verb) carries raw bytes — call it on the binary channel "
                            + "(sendWithBinary), not text send")
                ])
            default:
                return .object(["error": .string("unknown verb \(verb)")])
            }
        }
    }

    /// Binary channel: `read_stream` (empty request → reply BODY = one raw chunk)
    /// and `write_stream` (request BODY = one raw chunk → status reply, no body).
    /// Symmetric with py/rust's `bytes`-in-the-dict; never base64. Same
    /// sealed-by-default gate + running-dir clamp as the text channel. Any other
    /// verb routes through the text `handle` (its blob is unused).
    public func handleBinary(
        agentId: AgentId, header: JSON, blob: Data, kernel: Kernel
    ) async throws -> (JSON?, Data) {
        let verb = header["type"].asString ?? ""
        guard verb == "read_stream" || verb == "write_stream" else {
            let reply = try await handle(agentId: agentId, payload: header, kernel: kernel)
            return (reply, Data())
        }
        guard let agent = kernel.agent(agentId) else {
            return (.object(["error": .string("no agent \(agentId)")]), Data())
        }
        // GATE — same sealed-by-default choke point as the text channel.
        if let denied = gateVerb(
            ingressRule: agent.metaValue(forKey: "ingress_rule"),
            auth: agent.metaValue(forKey: "auth"),
            authToken: agent.metaValue(forKey: "auth_token")?.asString,
            agentId: agent.id.value, verb: verb)
        {
            return (denied, Data())
        }
        guard let root = clampedRoot(agent: agent, kernel: kernel) else {
            return (
                .object([
                    "error": .string("root escapes the running dir"),
                    "reason": .string("root_escapes_running_dir"),
                ]), Data()
            )
        }
        if verb == "read_stream" {
            return readStream(root: root, header: header)
        }
        if agent.metaValue(forKey: "readonly")?.asBool == true {
            return (.object(["error": .string("agent is readonly")]), Data())
        }
        return (writeStream(root: root, header: header, blob: blob), Data())
    }

    // MARK: - Verbs

    private func reflect(agent: Agent) -> JSON {
        let root = rootPath(agent: agent)
        let readonly = agent.metaValue(forKey: "readonly")?.asBool ?? false
        return [
            "id": .string(agent.id.value),
            "sentence": .string("Filesystem agent rooted at \(root.path)."),
            "root": .string(root.path),
            "readonly": .bool(readonly),
            "verbs": [
                "list": "args: path?. Returns {path, files}.",
                "read": "args: path. Returns {path, content} or {path, image_base64, mime}.",
                "write": "args: path, content. Returns {path, written: true}.",
                "delete": "args: path. Returns {path, deleted: true}.",
                "rename": "args: old_path, new_path. Returns {old_path, new_path}.",
                "mkdir": "args: path. Returns {path, created: true}.",
            ],
        ] as JSON
    }

    private func listFiles(root: URL, payload: JSON) -> JSON {
        let relative = payload["path"].asString ?? ""
        guard let target = resolve(root: root, path: relative) else {
            return .object(["error": .string("path escapes root")])
        }
        let fm = FileManager.default
        guard let entries = try? fm.contentsOfDirectory(
            at: target, includingPropertiesForKeys: [.isDirectoryKey, .fileSizeKey],
            options: [.skipsHiddenFiles])
        else {
            return .object([
                "error": .string("cannot list \(relative)"),
                "path": .string(relative),
            ])
        }
        var files: [JSON] = []
        for url in entries {
            let name = url.lastPathComponent
            if DEFAULT_HIDDEN.contains(name) { continue }
            let resourceValues = try? url.resourceValues(forKeys: [
                .isDirectoryKey, .fileSizeKey,
            ])
            let isDir = resourceValues?.isDirectory ?? false
            let rel = (relative.isEmpty ? name : "\(relative)/\(name)")
            var entry: OrderedDictionary<String, JSON> = [:]
            entry["name"] = .string(name)
            entry["path"] = .string(rel)
            entry["type"] = .string(isDir ? "dir" : "file")
            if !isDir, let size = resourceValues?.fileSize {
                entry["size"] = .integer(Int64(size))
            }
            files.append(.object(entry))
        }
        files.sort {
            ($0["name"].asString ?? "") < ($1["name"].asString ?? "")
        }
        return .object([
            "path": .string(relative),
            "files": .array(files),
        ])
    }

    private func readFile(root: URL, payload: JSON) -> JSON {
        guard let relative = payload["path"].asString else {
            return .object(["error": .string("read requires path")])
        }
        guard let target = resolve(root: root, path: relative) else {
            return .object(["error": .string("path escapes root")])
        }
        let ext = target.pathExtension.lowercased()
        if IMAGE_EXTENSIONS.contains(ext) {
            guard let data = try? Data(contentsOf: target) else {
                return .object([
                    "error": .string("cannot read \(relative)"),
                    "path": .string(relative),
                ])
            }
            return .object([
                "path": .string(relative),
                "image_base64": .string(data.base64EncodedString()),
                "mime": .string("image/\(ext)"),
            ])
        }
        guard let content = try? String(contentsOf: target, encoding: .utf8) else {
            return .object([
                "error": .string("cannot read \(relative) as utf-8"),
                "path": .string(relative),
            ])
        }
        return .object([
            "path": .string(relative),
            "content": .string(content),
        ])
    }

    private func writeFile(root: URL, agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        guard let relative = payload["path"].asString,
            let content = payload["content"].asString
        else {
            return .object(["error": .string("write requires path + content")])
        }
        guard let target = resolve(root: root, path: relative) else {
            return .object(["error": .string("path escapes root")])
        }
        try? FileManager.default.createDirectory(
            at: target.deletingLastPathComponent(),
            withIntermediateDirectories: true)
        do {
            try content.write(to: target, atomically: true, encoding: .utf8)
            return .object([
                "path": .string(relative),
                "written": .bool(true),
            ])
        } catch {
            return .object(["error": .string("write failed: \(error)")])
        }
    }

    private func deleteFile(root: URL, agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        guard let relative = payload["path"].asString else {
            return .object(["error": .string("delete requires path")])
        }
        guard let target = resolve(root: root, path: relative) else {
            return .object(["error": .string("path escapes root")])
        }
        do {
            try FileManager.default.removeItem(at: target)
            return .object([
                "path": .string(relative),
                "deleted": .bool(true),
            ])
        } catch {
            return .object(["error": .string("delete failed: \(error)")])
        }
    }

    private func renameFile(root: URL, agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        guard let oldRel = payload["old_path"].asString,
            let newRel = payload["new_path"].asString
        else {
            return .object(["error": .string("rename requires old_path + new_path")])
        }
        guard let oldURL = resolve(root: root, path: oldRel),
            let newURL = resolve(root: root, path: newRel)
        else {
            return .object(["error": .string("path escapes root")])
        }
        do {
            try FileManager.default.moveItem(at: oldURL, to: newURL)
            return .object([
                "old_path": .string(oldRel),
                "new_path": .string(newRel),
            ])
        } catch {
            return .object(["error": .string("rename failed: \(error)")])
        }
    }

    private func makeDir(root: URL, agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        guard let relative = payload["path"].asString else {
            return .object(["error": .string("mkdir requires path")])
        }
        guard let target = resolve(root: root, path: relative) else {
            return .object(["error": .string("path escapes root")])
        }
        do {
            try FileManager.default.createDirectory(
                at: target, withIntermediateDirectories: true)
            return .object([
                "path": .string(relative),
                "created": .bool(true),
            ])
        } catch {
            return .object(["error": .string("mkdir failed: \(error)")])
        }
    }

    // MARK: - Helpers

    /// The configured root verbatim (reflect shows this; data verbs clamp it).
    private func rootPath(agent: Agent) -> URL {
        if let rootStr = agent.metaValue(forKey: "root")?.asString {
            return URL(fileURLWithPath: rootStr)
        }
        return agent.rootPath
    }

    /// The kernel's RUNNING DIRECTORY — its disk workdir (the dir that holds
    /// `.fantastic`). In production this is the process cwd, matching py
    /// `Path.cwd()`; for an in-memory kernel that still touches disk we fall
    /// back to the process cwd.
    private func workdirBase(kernel: Kernel) -> URL {
        if let wd = kernel.storage.workdir { return wd }
        return URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
    }

    /// THE RUNNING-DIR LAW — clamp the configured root to the running dir
    /// (`base`). A relative root resolves under it; an absolute root is allowed
    /// ONLY if it lies inside it; outside escapes refuse. Mirrors py
    /// `fs.resolve_root` (whose base is `Path.cwd()`). Returns the clamped root,
    /// or nil on escape.
    private func clampedRoot(agent: Agent, kernel: Kernel) -> URL? {
        let base = workdirBase(kernel: kernel).standardizedFileURL
        let rawStr = agent.metaValue(forKey: "root")?.asString
        let candidate: URL
        if let rawStr, !rawStr.isEmpty {
            // Test the raw STRING for absoluteness — `URL(fileURLWithPath:)` on a
            // relative path resolves it against the process cwd, which would
            // wrongly read a workdir-relative root (".fantastic") as outside.
            candidate =
                rawStr.hasPrefix("/")
                ? URL(fileURLWithPath: rawStr).standardizedFileURL
                : base.appendingPathComponent(rawStr).standardizedFileURL
        } else {
            // No root meta ⇒ the agent's own dir (already inside the workdir).
            candidate = agent.rootPath.standardizedFileURL
        }
        // Must lie inside the running dir.
        if candidate.path == base.path || candidate.path.hasPrefix(base.path + "/") {
            return candidate
        }
        return nil
    }

    private func resolve(root: URL, path: String) -> URL? {
        let combined = root.appendingPathComponent(path).standardizedFileURL
        let canonicalRoot = root.standardizedFileURL.path
        let canonicalCombined = combined.path
        // Refuse if the resolved path escapes root.
        if !canonicalCombined.hasPrefix(canonicalRoot) { return nil }
        return combined
    }

    private func readonlyError(agent: Agent) -> JSON? {
        if agent.metaValue(forKey: "readonly")?.asBool == true {
            return .object(["error": .string("agent is readonly")])
        }
        return nil
    }

    // MARK: - Streams (binary channel)

    /// SOURCE — read ONE raw chunk at `offset`. Reply `{path, offset,
    /// next_offset, eof, size}` + body = the raw bytes (binary channel; never
    /// base64). Stateless cursor: pull the next with `offset=next_offset` to eof.
    private func readStream(root: URL, header: JSON) -> (JSON, Data) {
        let path = header["path"].asString ?? ""
        let offset = UInt64(header["offset"].asInt ?? 0)
        let length = header["length"].asInt.map { $0 > 0 ? $0 : 65536 } ?? 65536
        guard let target = resolve(root: root, path: path) else {
            return (.object(["error": .string("path escapes root")]), Data())
        }
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: target.path, isDirectory: &isDir),
            !isDir.boolValue
        else {
            return (.object(["error": .string("file \(path) not found")]), Data())
        }
        guard let handle = try? FileHandle(forReadingFrom: target) else {
            return (.object(["error": .string("cannot open \(path)")]), Data())
        }
        defer { try? handle.close() }
        let size =
            (try? FileManager.default.attributesOfItem(atPath: target.path)[.size] as? Int)
            .flatMap { $0 } ?? 0
        try? handle.seek(toOffset: offset)
        let chunk = (try? handle.read(upToCount: Int(length))) ?? Data()
        let nextOffset = offset + UInt64(chunk.count)
        let meta: JSON = .object([
            "path": .string(path),
            "offset": .integer(Int64(offset)),
            "next_offset": .integer(Int64(nextOffset)),
            "eof": .bool(nextOffset >= UInt64(size)),
            "size": .integer(Int64(size)),
            "bytes_len": .integer(Int64(chunk.count)),
        ])
        return (meta, chunk)
    }

    /// SINK — write ONE raw chunk (`blob`) at `offset` (default: append at end).
    /// `truncate:true` on the first chunk starts fresh. Returns
    /// `{path, written, offset, size}`.
    private func writeStream(root: URL, header: JSON, blob: Data) -> JSON {
        let path = header["path"].asString ?? ""
        let truncate = header["truncate"].asBool ?? false
        guard let target = resolve(root: root, path: path) else {
            return .object(["error": .string("path escapes root")])
        }
        try? FileManager.default.createDirectory(
            at: target.deletingLastPathComponent(), withIntermediateDirectories: true)
        if !FileManager.default.fileExists(atPath: target.path) {
            FileManager.default.createFile(atPath: target.path, contents: nil)
        }
        guard let handle = try? FileHandle(forWritingTo: target) else {
            return .object(["error": .string("cannot open \(path) for writing")])
        }
        defer { try? handle.close() }
        if truncate { try? handle.truncate(atOffset: 0) }
        let off: UInt64
        if let o = header["offset"].asInt {
            off = UInt64(o)
            try? handle.seek(toOffset: off)
        } else {
            off = (try? handle.seekToEnd()) ?? 0
        }
        do { try handle.write(contentsOf: blob) } catch {
            return .object(["error": .string("write \(path): \(error)")])
        }
        let size = (try? handle.offset()) ?? (off + UInt64(blob.count))
        return .object([
            "path": .string(path),
            "written": .integer(Int64(blob.count)),
            "offset": .integer(Int64(off)),
            "size": .integer(Int64(size)),
        ])
    }

    /// The PUMP — a server-side SOURCE→SINK copy, chunk by chunk, in ONE call.
    /// Storage-agnostic: both ends are bound BY ID + the duck-typed stream verbs
    /// over the binary channel, so a `network_bridge` SOURCE pumps to a
    /// `file_bridge` SINK the same as fs→fs. Each end SELF-gates + SELF-clamps.
    private func pump(agent: Agent, payload: JSON, kernel: Kernel) async -> JSON {
        let selfId = agent.id.value
        let src = payload["source"].asString ?? selfId
        let sink = payload["sink"].asString ?? selfId
        let spath = payload["source_path"].asString ?? payload["path"].asString ?? ""
        let dpath = payload["sink_path"].asString ?? spath
        let chunk = payload["chunk"].asInt.map { $0 > 0 ? $0 : 65536 } ?? 65536
        var offset = 0
        var first = true
        var chunks = 0
        while true {
            let (rmeta, body) = await kernel.sendWithBinary(
                AgentId(src),
                .object([
                    "type": .string("read_stream"), "path": .string(spath),
                    "offset": .integer(Int64(offset)), "length": .integer(Int64(chunk)),
                ]), Data())
            if rmeta["error"].asString != nil {
                return .object(["error": .string("pump: read from \(src) failed: \(rmeta)")])
            }
            let (wmeta, _) = await kernel.sendWithBinary(
                AgentId(sink),
                .object([
                    "type": .string("write_stream"), "path": .string(dpath),
                    "offset": .integer(Int64(offset)), "truncate": .bool(first),
                ]), body)
            if wmeta["error"].asString != nil {
                return .object(["error": .string("pump: write to \(sink) failed: \(wmeta)")])
            }
            first = false
            chunks += 1
            offset = Int(rmeta["next_offset"].asInt ?? Int64(offset))
            if rmeta["eof"].asBool ?? true { break }
        }
        return .object([
            "source": .string(spath), "sink": .string(dpath),
            "bytes": .integer(Int64(offset)), "chunks": .integer(Int64(chunks)),
        ])
    }
}
