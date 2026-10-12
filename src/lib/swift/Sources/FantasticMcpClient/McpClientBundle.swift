// mcp_client — ONE remote MCP server as an agent (owner, 2026-10-08). Python
// `mcp_client/tools.py`, text for text.
//
// `create_agent handler_module=mcp_client.tools url=<the server's MCP endpoint>
// credentials={store, ref}` (optional — a keyless server takes none). Its `boot`
// asks the server who it is (`server/discover`) and lists its tools (`tools/list`,
// every page); from then on EACH TOOL IS A VERB of this agent: `send {type: <tool>,
// …its arguments}` is a `tools/call`. `docs of=<this agent>` lists them — the
// agent's own table (`instanceVerbDocs`), built from each tool's `inputSchema` and
// `description`. A grant names tools like verbs.
//
// Streamable HTTP, DUAL-ERA (`McpWire`): protocol 2026-07-28 (stateless) first; a
// server that speaks only an `initialize`-era version (2025-11-25, 2025-06-18,
// 2025-03-26, 2024-11-05) is spoken to in that version, through its session.
// `reflect` names the version agreed. AUTH = a static key: the ref's `api_key`,
// read from the credential store before EVERY request and sent as
// `Authorization: Bearer` to the ref's host only.
//
// THE ANSWER: `{content, structured?}` — image/audio `data` and an embedded
// resource's `blob` arrive as RAW BYTES, ONE binary body per reply at its top-level
// `bytes` (the frame codec and every leg carry one, there): the first item says
// `body: "bytes"`, a later one has `size` and `omitted`. `isError` adds `error` (settles `errored`); a tool that
// needs input mid-call answers `{error, input_required}`.

import FantasticAICore
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public let MCP_CLIENT_HANDLER_MODULE = "mcp_client.tools"
/// What a binary item past the first carries instead of its bytes.
public let MCP_OMITTED = "one binary body per reply — this item's bytes were not sent"

private let REFLECT_DOC =
    "No args. → {id, sentence, url, protocol_version (the one agreed at boot; null before), server?, instructions?, tools, skipped?, last_error?}."
private let BOOT_DOC =
    "No args. Ask the server who it is and list its tools — this agent's verbs. Returns {server, protocol_version, tools:[names], skipped?}.\n\nSent again it RE-LISTS (a server's tools changed). A failure keeps the tools\nalready listed and answers why; `reflect` shows it as `last_error`."
private let NOTES =
    "A remote MCP server's tools are this agent's verbs (`docs of=<id>`). Record: `url` (the MCP endpoint), `credentials` ({store, ref} holding `api_key`, sent as a Bearer key to that host only; absent = keyless). Wiring: this agent's `can_call` names the store, the store's `callable_by` names this agent. A tool runs on the remote server with the key's rights — the grant on each tool is the consent."

/// What one server said, per instance.
struct McpState: Sendable {
    var tools: [(String, JSON)]? = nil
    var docs: [(String, String)] = []
    var mirrors: [String: [McpWire.Mirror]] = [:]
    var server: JSON = .null
    var instructions: JSON = .null
    var skipped: [JSON] = []
    var error: String? = nil
}

public final class McpClientBundle: AgentBundle, @unchecked Sendable {
    public let name = "mcp_client"
    public var verbs: [String] { ["reflect", "boot"] }
    public var verbDocTable: JSON {
        .object(["reflect": .string(REFLECT_DOC), "boot": .string(BOOT_DOC)])
    }
    public var emitsDoc: JSON? {
        .object([
            "tools_listed": .string(
                "{tools:[names], skipped:[{name, why}]} — after every listing (boot)"),
            "progress": .string(
                "{tool, progressToken, progress, total?, message?} — a tool's progress"),
            "log": .string("{tool, level, data, logger?} — a log line a tool sent"),
        ])
    }
    public var notesDoc: JSON? { .string(NOTES) }
    public var recordSchema: JSON? { Self.schema }

    static let schema: JSON = .object([
        "fields": .array([
            RecordSchema.field(
                "url", "str", "the MCP server's endpoint, e.g. https://mcp.example.com/mcp",
                required: true),
            RecordSchema.credentialsField(required: false),
        ])
    ])

    private let lock = NSLock()
    private var state: [String: McpState] = [:]
    /// What each instance agreed with its server (era, version, session id).
    private var sessions: [String: McpSession] = [:]

    public init() {}

    private func get(_ id: String) -> McpState {
        lock.lock()
        defer { lock.unlock() }
        return state[id] ?? McpState()
    }

    private func session(_ id: String) -> McpSession {
        lock.lock()
        defer { lock.unlock() }
        return sessions[id] ?? McpSession()
    }

    private func setSession(_ id: String, _ s: McpSession) {
        lock.lock()
        defer { lock.unlock() }
        sessions[id] = s
    }

    private func put(_ id: String, _ f: (inout McpState) -> Void) {
        lock.lock()
        defer { lock.unlock() }
        var s = state[id] ?? McpState()
        f(&s)
        state[id] = s
    }

    /// Verbs the kernel and this bundle answer first — a tool by one of these
    /// names is skipped, never shadowed silently.
    static func reserved(_ name: String) -> Bool {
        ["reflect", "boot", "shutdown"].contains(name) || Kernel.kernelVerbDocs()[name] != .null
    }

    public func instanceVerbDocs(_ agent: Agent) -> JSON? {
        var out: OrderedDictionary<String, JSON> = [
            "reflect": .string(REFLECT_DOC), "boot": .string(BOOT_DOC),
        ]
        for (n, d) in get(agent.id.value).docs { out[n] = .string(d) }
        return .object(out)
    }

    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        if let url = meta["url"], url != .null {
            var ok = false
            if case .string(let s) = url, let u = URLComponents(string: s), let scheme = u.scheme {
                ok =
                    ["http", "https"].contains(scheme.lowercased()) && !(u.host ?? "").isEmpty
                    && u.fragment == nil
            }
            if !ok {
                return
                    "url=\(pyRepr(url)) must be an absolute http(s) URL of the MCP endpoint, with no fragment (e.g. https://mcp.example.com/mcp)"
            }
        }
        return RecordSchema.validate(Self.schema, backend: "mcp_client", meta: meta, record: record)
    }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        try await handleBinary(agentId: agentId, header: payload, body: nil, kernel: kernel).0
    }

    public func handleBinary(
        agentId: AgentId, header: JSON, body: Blob?, kernel: Kernel
    ) async throws -> (JSON?, Blob?) {
        let verb = header["type"].asString ?? ""
        switch verb {
        case "reflect": return (reflect(agentId, kernel), nil)
        case "boot": return (await boot(agentId, kernel), nil)
        default: break
        }
        let id = agentId.value
        let st = get(id)
        guard let tools = st.tools else {
            return (
                Refusal.make(
                    Refusal.notConnected,
                    "\(id): the server's tools are not listed — \(st.error ?? "boot has not run")",
                    hint: "send boot to \(id) once the server is up"), nil
            )
        }
        guard tools.contains(where: { $0.0 == verb }) else {
            let names = tools.map(\.0)
            return (
                Refusal.make(
                    Refusal.unhandled, "\(id): the MCP server has no tool \(pyRepr(verb))",
                    hint:
                        "its tools: \(names.isEmpty ? "none" : names.joined(separator: ", ")) (docs of=\(id)); boot re-lists them",
                    extra: [("verb", header["type"])]), nil
            )
        }
        do {
            return try await call(agentId, verb, header, kernel, st.mirrors[verb] ?? [])
        } catch {
            return (error.reply, nil)
        }
    }

    public func onDelete(agentId: AgentId, kernel: Kernel) async throws {
        forget(agentId.value)
    }

    private func forget(_ id: String) {
        lock.lock()
        defer { lock.unlock() }
        state[id] = nil
        sessions[id] = nil
    }

    // ── the verbs ────────────────────────────────────────────────────

    private func url(_ id: AgentId, _ kernel: Kernel) -> String {
        kernel.agent(id)?.metaValue(forKey: "url")?.asString ?? ""
    }

    private func key(_ id: AgentId, _ kernel: Kernel, _ url: String) async throws(McpError)
        -> String?
    {
        do {
            return try await keyReader(id, kernel, backend: "mcp_client", required: false)(url)
        } catch {
            let text = (error as? PyError)?.message ?? "\(error)"
            throw McpError(Refusal.make(Refusal.unauthorized, text))
        }
    }

    private func reflect(_ id: AgentId, _ kernel: Kernel) -> JSON {
        let url = kernel.agent(id)?.metaValue(forKey: "url") ?? .null
        let st = get(id.value)
        var out: OrderedDictionary<String, JSON> = [
            "id": .string(id.value),
            "sentence": .string(
                "The MCP server at \(url == .null ? "None" : pyStr(url)) — its tools are this agent's verbs (docs of=\(id.value))."
            ),
            "url": url,
            "protocol_version": session(id.value).version.map(JSON.string) ?? .null,
            "tools": .integer(Int64(st.tools?.count ?? 0)),
        ]
        if pyTruthy(st.server) { out["server"] = st.server }
        if pyTruthy(st.instructions) { out["instructions"] = st.instructions }
        if !st.skipped.isEmpty { out["skipped"] = .array(st.skipped) }
        if let e = st.error, !e.isEmpty { out["last_error"] = .string(e) }
        return .object(out)
    }

    private func boot(_ id: AgentId, _ kernel: Kernel) async -> JSON {
        let u = url(id, kernel)
        do {
            return try await list(id, kernel, u)
        } catch {
            let err = error.reply["error"]
            put(id.value) { $0.error = err == .null ? nil : pyStr(err) }
            return error.reply
        }
    }

    private func list(_ id: AgentId, _ kernel: Kernel, _ url: String) async throws(McpError) -> JSON
    {
        let k = try await key(id, kernel, url)
        var sess = session(id.value)
        let disc: OrderedDictionary<String, JSON>
        do {
            disc = try await McpWire.connect(url, key: k, &sess)
            setSession(id.value, sess)
        } catch {
            setSession(id.value, sess)
            throw error
        }
        var tools: [JSON] = []
        if disc["capabilities"]?.asObject?["tools"] != nil {
            var cursor: JSON? = nil
            var seen: [JSON] = []
            while true {
                var params: OrderedDictionary<String, JSON> = [:]
                if let c = cursor { params["cursor"] = c }
                let page: OrderedDictionary<String, JSON>
                do {
                    page = try await McpWire.rpc(url, "tools/list", params, key: k, session: &sess)
                    setSession(id.value, sess)
                } catch {
                    setSession(id.value, sess)
                    throw error
                }
                tools += page["tools"]?.asArray ?? []
                let next = page["nextCursor"] ?? .null
                guard pyTruthy(next) else { break }
                if seen.contains(next) {
                    throw McpError(
                        Refusal.make(
                            Refusal.transportError,
                            "mcp \(url): tools/list repeated cursor \(pyRepr(next))"))
                }
                seen.append(next)
                cursor = next
            }
        }
        var table: [(String, JSON)] = []
        var docs: [(String, String)] = []
        var mirrors: [String: [McpWire.Mirror]] = [:]
        var skipped: [JSON] = []
        for t in tools {
            let name = t["name"]
            guard case .string(let n) = name, !n.isEmpty else {
                skipped.append(.object(["name": name, "why": .string("a tool with no name")]))
                continue
            }
            if Self.reserved(n) {
                skipped.append(
                    .object([
                        "name": .string(n),
                        "why": .string("the kernel or this bundle answers that verb"),
                    ]))
                continue
            }
            if table.contains(where: { $0.0 == n }) {
                skipped.append(.object(["name": .string(n), "why": .string("listed twice")]))
                continue
            }
            let schema = t["inputSchema"] == .null ? JSON.object([:]) : t["inputSchema"]
            // `x-mcp-header` is a 2026-07-28 rule: a legacy server's schema mirrors nothing
            let found: Result<[McpWire.Mirror], McpReason> =
                sess.era == .modern ? McpWire.mirrors(schema) : .success([])
            switch found {
            case .failure(let why):
                skipped.append(.object(["name": .string(n), "why": .string(why.text)]))
                continue
            case .success(let found):
                mirrors[n] = found
            }
            docs.append((n, Self.toolDoc(t)))
            table.append((n, t))
        }
        let server = disc["server"] ?? .null
        let names = table.map(\.0)
        let instructions = disc["instructions"] ?? .null
        put(id.value) { s in
            s = McpState(
                tools: table, docs: docs, mirrors: mirrors, server: server,
                instructions: instructions,
                skipped: skipped, error: nil)
        }
        await kernel.emit(
            id,
            .object([
                "type": .string("tools_listed"), "tools": .array(names.map { .string($0) }),
                "skipped": .array(skipped),
            ]))
        var out: OrderedDictionary<String, JSON> = [
            "server": server, "protocol_version": sess.version.map(JSON.string) ?? .null,
            "tools": .array(names.map { .string($0) }),
        ]
        if !skipped.isEmpty { out["skipped"] = .array(skipped) }
        return .object(out)
    }

    private func call(
        _ id: AgentId, _ tool: String, _ payload: JSON, _ kernel: Kernel,
        _ mirrors: [McpWire.Mirror]
    ) async throws(McpError) -> (JSON?, Blob?) {
        let u = url(id, kernel)
        var arguments: OrderedDictionary<String, JSON> = [:]
        for (k, v) in payload.asObject ?? [:] where k != "type" { arguments[k] = v }
        let k = try await key(id, kernel, u)
        let headers = try McpWire.paramHeaders(mirrors, .object(arguments))
        var sess = session(id.value)
        defer { setSession(id.value, sess) }
        let result = try await McpWire.rpc(
            u, "tools/call", ["name": .string(tool), "arguments": .object(arguments)], key: k,
            session: &sess, headers: headers
        ) { msg in
            let kind: String
            switch msg["method"].asString {
            case "notifications/progress": kind = "progress"
            case "notifications/message": kind = "log"
            default: return
            }
            var out: OrderedDictionary<String, JSON> = [
                "type": .string(kind), "tool": .string(tool),
            ]
            for (pk, pv) in msg["params"].asObject ?? [:] { out[pk] = pv }
            await kernel.emit(id, .object(out))
        }
        return try Self.answer(u, tool, result)
    }

    // ── the answer ───────────────────────────────────────────────────

    static func b64(_ url: String, _ s: JSON) throws(McpError) -> Data {
        guard case .string(let text) = s, let d = Data(base64Encoded: text) else {
            throw McpError(
                Refusal.make(Refusal.transportError, "mcp \(url): invalid base64 in a content item")
            )
        }
        return d
    }

    /// A `tools/call` result as the caller reads it — the JSON and its one body.
    static func answer(_ url: String, _ tool: String, _ result: OrderedDictionary<String, JSON>)
        throws(McpError) -> (JSON?, Blob?)
    {
        if result["resultType"]?.asString == "input_required" {
            var ir: OrderedDictionary<String, JSON> = [:]
            for k in ["inputRequests", "requestState"] { if let v = result[k] { ir[k] = v } }
            return (
                .object([
                    "error": .string("\(tool): the server needs input this client cannot give yet"),
                    "input_required": .object(ir),
                ]), nil
            )
        }
        var sent: Blob? = nil
        // ONE binary body, at the reply's top-level `bytes` (the frame codec and
        // every leg carry one, there): the first item names it, a later one is omitted
        func body(_ data: JSON) throws(McpError) -> OrderedDictionary<String, JSON> {
            let raw = try b64(url, data)
            if sent != nil {
                return ["size": .integer(Int64(raw.count)), "omitted": .string(MCP_OMITTED)]
            }
            sent = Blob("bytes", raw)
            return ["size": .integer(Int64(raw.count)), "body": "bytes"]
        }
        var content: [JSON] = []
        for c in result["content"]?.asArray ?? [] {
            guard let m = c.asObject else {
                content.append(c)
                continue
            }
            let kind = m["type"]?.asString ?? ""
            if ["image", "audio"].contains(kind), case .string = m["data"] ?? .null {
                var rest = m
                rest["data"] = nil
                for (k, v) in try body(m["data"]!) { rest[k] = v }
                content.append(.object(rest))
                continue
            }
            if kind == "resource", var res = m["resource"]?.asObject, let blob = res["blob"] {
                res["blob"] = nil
                for (k, v) in try body(blob) {
                    res[k] = v
                }
                var item = m
                item["resource"] = .object(res)
                content.append(.object(item))
                continue
            }
            content.append(c)
        }
        var out: OrderedDictionary<String, JSON> = [:]
        if result["isError"] == .bool(true) {
            let text = content.filter { $0["type"].asString == "text" }.compactMap {
                $0["text"].asString
            }
            .joined(separator: "\n")
            out["error"] = .string(text.isEmpty ? "\(tool): the tool reported an error" : text)
        }
        out["content"] = .array(content)
        if let s = result["structuredContent"] { out["structured"] = s }
        return (.object(out), sent)
    }

    // ── a tool's doc, from its schema ────────────────────────────────

    static func typeName(_ s: JSON) -> String {
        let names = [
            "string": "str", "integer": "int", "number": "float", "boolean": "bool",
            "array": "list",
            "object": "dict", "null": "null",
        ]
        switch s["type"] {
        case .array(let ts):
            return ts.map { t in t.asString.map { names[$0] ?? $0 } ?? pyStr(t) }.joined(
                separator: "|")
        case .string(let t): return names[t] ?? "any"
        default: return "any"
        }
    }

    /// `args: a:str (req), b:int?. <description>` — the signature first (what the
    /// oneliner tier keeps), the tool's description after it.
    static func toolDoc(_ tool: JSON) -> String {
        let schema = tool["inputSchema"]
        let req = Set((schema["required"].asArray ?? []).compactMap(\.asString))
        let sig: String
        if let props = schema["properties"].asObject, !props.isEmpty {
            let args = props.map { n, s in
                "\(n):\(typeName(s))" + (req.contains(n) ? " (req)" : "?")
            }
            sig = "args: \(args.joined(separator: ", "))."
        } else {
            sig = "No args."
        }
        let d = tool["description"].asString ?? ""
        let desc = (d.isEmpty ? (tool["title"].asString ?? "") : d).trimmingCharacters(
            in: .whitespacesAndNewlines)
        return desc.isEmpty ? sig : "\(sig) \(desc)"
    }
}
