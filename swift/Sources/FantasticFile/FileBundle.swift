// Filesystem-as-agent bundle.
//
// Mirrors Rust's `fantastic-file::FileBundle`. Each agent owns a
// `root` directory (from agent meta) and a `readonly` flag; every
// verb resolves the supplied path within `root` and refuses if the
// canonical result escapes (`..` traversal protection).
//
// Verbs: reflect / boot / list / read / write / delete / rename / mkdir.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let HANDLER_MODULE = "file.tools"

/// Default hidden-file names hidden from `list`.
let DEFAULT_HIDDEN: Set<String> = [
    ".git", ".env", ".fantastic", "node_modules", "__pycache__",
]

let IMAGE_EXTENSIONS: Set<String> = [
    "png", "jpg", "jpeg", "gif", "webp", "svg",
]

public struct FileBundle: AgentBundle {
    public let name = "file"

    public init() {}

    public var readme: String? {
        "file — filesystem as agent. Each agent owns `root`; verbs are path-safe."
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
        switch verb {
        case "reflect":
            return reflect(agent: agent)
        case "boot":
            return .object(["ok": .bool(true)])
        case "list":
            return listFiles(agent: agent, payload: payload)
        case "read":
            return readFile(agent: agent, payload: payload)
        case "write":
            return writeFile(agent: agent, payload: payload)
        case "delete":
            return deleteFile(agent: agent, payload: payload)
        case "rename":
            return renameFile(agent: agent, payload: payload)
        case "mkdir":
            return makeDir(agent: agent, payload: payload)
        default:
            return .object(["error": .string("unknown verb \(verb)")])
        }
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

    private func listFiles(agent: Agent, payload: JSON) -> JSON {
        let root = rootPath(agent: agent)
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

    private func readFile(agent: Agent, payload: JSON) -> JSON {
        let root = rootPath(agent: agent)
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

    private func writeFile(agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        let root = rootPath(agent: agent)
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

    private func deleteFile(agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        let root = rootPath(agent: agent)
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

    private func renameFile(agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        let root = rootPath(agent: agent)
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

    private func makeDir(agent: Agent, payload: JSON) -> JSON {
        if let ro = readonlyError(agent: agent) { return ro }
        let root = rootPath(agent: agent)
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

    private func rootPath(agent: Agent) -> URL {
        if let rootStr = agent.metaValue(forKey: "root")?.asString {
            return URL(fileURLWithPath: rootStr)
        }
        return agent.rootPath
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
}
