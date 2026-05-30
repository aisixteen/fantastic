// Interactive stdin REPL for an already-booted kernel.
//
// Mirrors `python/kernel/_modes.py:_repl_loop` verbatim. Composed by
// `DaemonMode.run` when no-args invocation runs in a tty context.
// Renamed from Chat.swift to drop the explicit `fantastic chat`
// subcommand — REPL is now implicit under no-args + tty, matching
// Python's `_default()` composition.
//
// Command grammar (Python parity):
//
//   list                          all agents
//   add <bundle> [k=v ...]        create_agent + boot
//   delete <id>                   delete_agent
//   @<id>                         send {type:"send", text:""} to <id>
//   @<id> <verb>                  send {type:"<verb>"}             — RPC
//   @<id> <verb> k=v [k=v ...]    send {type:"<verb>", k:v, ...}   — RPC
//   @<id> <multi-word prose>      send {type:"send", text:"<prose>"} — chat
//   exit / quit / Ctrl-D          break
//
// Streaming output (token events from LLM backends) is NOT rendered
// by the REPL itself. The REPL prints the immediate `send` reply
// (typically `{queued: true, stream_id: ...}`) and returns to the
// prompt. To see streamed tokens, wire a `cli` bundle that watches
// the LLM agent and renders events to stdout — same pattern Python
// uses with its `cli` renderer agent. Not in scope here.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

enum REPLMode {

    /// Run the REPL against an already-booted kernel. Returns when
    /// the user hits Ctrl-D / types `exit` / types `quit`. The caller
    /// (typically `DaemonMode`) is responsible for kernel lifecycle.
    static func run(kernel: Kernel) async {
        while true {
            print("fantastic> ", terminator: "")
            guard let raw = readLine() else { break }  // Ctrl-D
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty { continue }
            if line == "exit" || line == "quit" { break }

            if line == "list" {
                await handleList(kernel: kernel)
                continue
            }
            if line.hasPrefix("add ") {
                await handleAdd(
                    args: String(line.dropFirst(4)), kernel: kernel)
                continue
            }
            if line.hasPrefix("delete ") {
                await handleDelete(
                    id: String(
                        line.dropFirst(7).trimmingCharacters(in: .whitespaces)),
                    kernel: kernel)
                continue
            }
            if line.hasPrefix("@") {
                await handleAt(line: line, kernel: kernel)
                continue
            }
            printLine(
                "  unknown command: \(line.debugDescription)  "
                + "(try: list, add <bundle>, delete <id>, @<id> ...)"
            )
        }
        printLine("")
    }

    // MARK: - Commands

    private static func handleList(kernel: Kernel) async {
        let reply = await kernel.send(
            AgentId("core"), .object(["type": .string("list_agents")]))
        for agent in reply["agents"].asArray ?? [] {
            let id = agent["id"].asString ?? "?"
            let hm = agent["handler_module"].asString ?? "<root>"
            printLine("  \(id)  →  \(hm)")
        }
    }

    private static func handleAdd(args raw: String, kernel: Kernel) async {
        let parts = splitArgs(raw)
        guard let bundle = parts.first else {
            printLine("  usage: add <bundle> [k=v ...]")
            return
        }
        // Convention: `<bundle>` → `<bundle>.tools` if no dot present.
        // Matches Python's `_find_bundle_module` resolution.
        let handlerModule = bundle.contains(".") ? bundle : "\(bundle).tools"

        var payload: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"),
            "handler_module": .string(handlerModule),
        ]
        for p in parts.dropFirst() {
            guard let eq = p.firstIndex(of: "=") else { continue }
            let k = String(p[..<eq])
            let v = String(p[p.index(after: eq)...])
            payload[k] = coerce(v)
        }
        let reply = await kernel.send(AgentId("core"), .object(payload))
        if let id = reply["id"].asString {
            _ = await kernel.send(AgentId(id), .object(["type": .string("boot")]))
            printLine("  added \(id) (\(handlerModule))")
        } else if let err = reply["error"].asString {
            printLine("  add failed: \(err)")
        } else {
            printLine(reply.serialize())
        }
    }

    private static func handleDelete(id: String, kernel: Kernel) async {
        if id.isEmpty {
            printLine("  usage: delete <id>")
            return
        }
        let reply = await kernel.send(
            AgentId("core"),
            .object([
                "type": .string("delete_agent"),
                "id": .string(id),
            ]))
        if let err = reply["error"].asString {
            printLine("  delete failed: \(err)")
        } else {
            printLine("  deleted \(id)")
        }
    }

    /// `@<id> ...` parser — strict Python parity (no chat-first
    /// heuristic). Mirrors `python/kernel/_modes.py:_parse_at`:
    ///
    ///   @<id>                       → {type:"send", text:""}
    ///   @<id> <single-token>        → {type:"<single-token>"}    (RPC)
    ///   @<id> <verb> k=v [k=v ...]  → {type:"<verb>", k:v, ...}  (RPC)
    ///   @<id> <multi-word prose>    → {type:"send", text:"<prose>"} (chat)
    ///
    /// Note: single-word inputs interpreted strictly as verbs means
    /// `@fm hi` yields `{type:"hi"}` which the fm bundle rejects with
    /// "unknown verb hi" — same behavior as Python's REPL. Chat with
    /// an LLM agent requires either explicit `send` verb or
    /// multi-word prose: `@fm send text="hi"` or
    /// `@fm tell me a joke`.
    private static func handleAt(line: String, kernel: Kernel) async {
        guard let cmd = parseAtCommand(line) else {
            printLine("  usage: @<id> <text> | @<id> <verb> k=v ...")
            return
        }
        await dispatchAndPrint(
            kernel: kernel, target: cmd.target, payload: cmd.payload)
    }

    /// Pure parse of a REPL `@`-line into a (target, payload) pair —
    /// no kernel side effects, so it's unit-testable. Returns nil for
    /// an empty `@` (caller prints usage). Strict Python-REPL parity:
    ///   `@id`               → {type:"send", text:""}
    ///   `@id <verb>`        → {type:"<verb>"}                 (single bare token)
    ///   `@id <verb> k=v...` → {type:"<verb>", k:v, ...}
    ///   `@id k=v...`        → {type:"send", k:v, ...}         (leading kv ⇒ verb=send)
    ///   `@id some words`    → {type:"send", text:"some words"} (whitespace preserved)
    static func parseAtCommand(_ line: String) -> (target: String, payload: JSON)? {
        let rest = String(line.dropFirst()).trimmingCharacters(in: .whitespaces)
        if rest.isEmpty {
            return nil
        }
        let parts = splitArgs(rest)
        let target = parts[0]
        let argTokens = Array(parts.dropFirst())

        // Empty args → send empty chat.
        if argTokens.isEmpty {
            return (
                target,
                .object([
                    "type": .string("send"),
                    "text": .string(""),
                ])
            )
        }

        // Single non-kv token → verb name (RPC), no args.
        if argTokens.count == 1 && !argTokens[0].contains("=") {
            return (target, .object(["type": .string(argTokens[0])]))
        }

        // Any kv tokens → verb + kv args (RPC). If first token has no
        // `=`, it's the verb name; otherwise verb defaults to "send".
        if argTokens.contains(where: { $0.contains("=") }) {
            let verb: String
            let kvTokens: [String]
            if argTokens[0].contains("=") {
                verb = "send"
                kvTokens = argTokens
            } else {
                verb = argTokens[0]
                kvTokens = Array(argTokens.dropFirst())
            }
            var payload: OrderedDictionary<String, JSON> = [
                "type": .string(verb)
            ]
            for t in kvTokens {
                guard let eq = t.firstIndex(of: "=") else { continue }
                let k = String(t[..<eq])
                let v = String(t[t.index(after: eq)...])
                payload[k] = coerce(v)
            }
            return (target, .object(payload))
        }

        // Multi-token, no `=` → chat send with the literal text after
        // the target. Reconstruct from the original line so internal
        // whitespace is preserved (not collapsed by splitArgs).
        let dropped = line.dropFirst()  // skip `@`
        guard let rangeOfTarget = dropped.range(of: target) else {
            return (
                target,
                .object([
                    "type": .string("send"),
                    "text": .string(""),
                ])
            )
        }
        let textPart = dropped[rangeOfTarget.upperBound...]
            .trimmingCharacters(in: .whitespaces)
        return (
            target,
            .object([
                "type": .string("send"),
                "text": .string(textPart),
            ])
        )
    }

    private static func dispatchAndPrint(
        kernel: Kernel, target: String, payload: JSON
    ) async {
        let reply = await kernel.send(AgentId(target), payload)
        // Match Python's `_print_result`: pretty-print non-null
        // replies; suppress null silently.
        if case .null = reply { return }
        printLine(reply.serializePretty(indent: 2))
    }

    // MARK: - Parsing helpers

    /// Split a line on whitespace, honoring double-quoted segments.
    /// Subset of `shlex.split` covering the cases the REPL exercises.
    private static func splitArgs(_ s: String) -> [String] {
        var out: [String] = []
        var cur = ""
        var inQuotes = false
        for ch in s {
            if ch == "\"" {
                inQuotes.toggle()
                continue
            }
            if ch.isWhitespace && !inQuotes {
                if !cur.isEmpty {
                    out.append(cur)
                    cur = ""
                }
                continue
            }
            cur.append(ch)
        }
        if !cur.isEmpty { out.append(cur) }
        return out
    }

    /// Coerce a string value into JSON. Mirrors Python's `_coerce`:
    ///   true/false → bool
    ///   integer    → integer
    ///   double     → double
    ///   else       → string
    private static func coerce(_ v: String) -> JSON {
        switch v.lowercased() {
        case "true": return .bool(true)
        case "false": return .bool(false)
        default: break
        }
        if let i = Int64(v) { return .integer(i) }
        if let d = Double(v) { return .double(d) }
        return .string(v)
    }

    private static func printLine(_ s: String) {
        print(s)
        fflush(stdout)
    }
}
