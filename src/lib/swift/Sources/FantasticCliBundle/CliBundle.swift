// cli — the terminal render sink.
//
// Swift mirror of the canonical python `cli` bundle (canonical; on disagreement
// this file is wrong). Receives `token` / `done` / `say` / `error` / `status` /
// `context` and prints them to stdout. Unknown verbs are silently DROPPED, not
// refused: a render surface that answered `unknown verb` would turn a cosmetic
// gap into a failed send for whoever was streaming to it.
//
// Renders the PTY intro too, but as a DUMB SINK — it prints what it is told and
// NEVER inspects the tree. The kernel/agents push to it:
//   - `intro_booting` (kernel → cli, before boot): identity + the pull/push
//     control-plane map (port-independent).
//   - each agent announces its OWN endpoints during its boot (e.g. `web` sends
//     a `say` with its listening URL) — the producer owns the info, not this
//     sink.
//   - `booted` (kernel → cli, after the boot loop): the "all booted" close.
// The kernel stays decoupled: it sends verbs, it never imports this.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

public struct CliBundle: AgentBundle {
    /// The handler module id a record names to select this bundle.
    public static let handlerModule = "cli.tools"

    /// The events this sink renders. Anything else is dropped.
    public static let accepts = ["token", "done", "say", "error", "status", "context"]

    /// The verb table `reflect` advertises — python's `VERBS`, each doc string
    /// the first line of the python handler's docstring, verbatim.
    static let verbDocs: OrderedDictionary<String, String> = [
        "reflect": "Identity + accepted event types. No args.",
        "token": "args: text:str. Writes to stdout (no newline). Returns None.",
        "done": "args: none. Emits a single newline; closes a token-stream cleanly. Returns None.",
        "say": "args: text:str, source:str?. Prints `  [source] text` on its own line. Returns None.",
        "error": "args: text:str. Prints `  ERROR: text` on its own line. Returns None.",
        "status":
            "args: phase:str, source:str?, detail:dict?. Renders one-line phase markers for queued / thinking / tool_calling (entry+exit). `streaming` and `done` produce no output (token + done handlers cover them). Returns None.",
        "context":
            "args: phase:str ('compacted'|'too_small'), source:str?, detail:dict?. Renders a one-line context-protocol notice.",
        "intro_booting":
            "First PTY push, BEFORE boot. Identity + the pull/push control-plane map — port-independent, so it prints instantly. No args. Returns None.",
        "booted": "Final PTY push — the kernel's \"all booted\" signal, sent AFTER the boot loop.",
    ]

    public let name = "cli"
    public var verbs: [String] { Array(Self.verbDocs.keys) }
    public var verbDocTable: JSON { .object(Self.verbDocs.mapValues { JSON.string($0) }) }

    public init() {}

    public var readme: String? { Self.readmeText }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        switch payload["type"].asString ?? "" {
        case "reflect":
            return Self.reflect(agentId)
        case "token":
            Self.write(try Self.tokenText(payload))
        case "done":
            Self.write("\n")
        case "say":
            Self.write(Self.renderSay(payload) + "\n")
        case "error":
            Self.write(Self.renderError(payload) + "\n")
        case "status":
            if let line = try Self.renderStatus(payload) { Self.write(line + "\n") }
        case "context":
            if let line = try Self.renderContext(payload) { Self.write(line + "\n") }
        case "intro_booting":
            Self.write(Self.introBootingLines(kernel).joined(separator: "\n") + "\n")
        case "booted":
            Self.write(Self.bootedLine + "\n")
        default:
            // cli is silent on unknown verbs (boot/shutdown included).
            break
        }
        return .null
    }

    // ─── reflect ────────────────────────────────────────────────

    static func reflect(_ agentId: AgentId) -> JSON {
        return .object([
            "id": .string(agentId.value),
            "sentence": .string("Terminal renderer. Prints incoming events to stdout."),
            "accepts": .array(accepts.map { .string($0) }),
        ])
    }

    // ─── rendering (pure: a line, or nil for silence) ───────────

    /// Python's `d.get(key, fallback)`: a MISSING key takes the fallback, a key
    /// present holding `null` is `None` and renders as `None`. The JSON
    /// subscript cannot tell the two apart, so every default here goes through
    /// key presence instead.
    static func get(_ d: JSON, _ key: String, _ fallback: JSON) -> JSON {
        d.asObject?[key] ?? fallback
    }

    /// Python's `payload.get("detail") or {}` followed by `.get(…)`: a falsy
    /// value becomes an empty dict, and a TRUTHY non-dict raises — python calls
    /// `.get` on it and gets an `AttributeError`, which the kernel stamps as the
    /// execution's error. There is no third branch that silently renders.
    static func mapping(_ v: JSON) throws(PyError) -> JSON {
        guard pyTruthy(v) else { return .object([:]) }
        guard case .object = v else {
            throw PyError("AttributeError", "'\(pyTypeName(v))' object has no attribute 'get'")
        }
        return v
    }

    /// `sys.stdout.write(payload.get("text", ""))`: a missing `text` writes
    /// nothing, and a PRESENT non-string is python's `TypeError` — the write
    /// never coerces, so neither does this.
    static func tokenText(_ payload: JSON) throws(PyError) -> String {
        let v = get(payload, "text", .string(""))
        guard case .string(let s) = v else {
            let name = v.isNull ? "None" : pyTypeName(v)
            throw PyError("TypeError", "write() argument must be str, not \(name)")
        }
        return s
    }

    static func text(_ payload: JSON) -> JSON { get(payload, "text", .string("")) }
    static func source(_ payload: JSON) -> JSON { get(payload, "source", .string("")) }

    /// `  [source] ` when the source is TRUTHY, else two spaces — python's
    /// `f"  [{src}] " if src else "  "`, so `0`, `""` and `None` all take the
    /// bare prefix while a non-string truthy source renders through `str()`.
    static func sayPrefix(_ src: JSON) -> String {
        pyTruthy(src) ? "  [\(pyStr(src))] " : "  "
    }

    /// `  [source]` / ` ` — the status/context variant, which puts its own
    /// space after the prefix. Python's two differ by exactly that trailing space.
    static func linePrefix(_ src: JSON) -> String {
        pyTruthy(src) ? "  [\(pyStr(src))]" : " "
    }

    static func renderSay(_ payload: JSON) -> String {
        sayPrefix(source(payload)) + pyStr(text(payload))
    }

    static func renderError(_ payload: JSON) -> String {
        "  ERROR: " + pyStr(text(payload))
    }

    static func renderStatus(_ payload: JSON) throws(PyError) -> String? {
        let phase = payload["phase"].asString ?? ""
        let detail = try mapping(get(payload, "detail", .null))
        let prefix = linePrefix(source(payload))
        switch phase {
        case "queued":
            return "\(prefix) queued (\(pyStr(get(detail, "ahead", .integer(0)))) ahead)"
        case "thinking":
            return "\(prefix) thinking…"
        case "tool_calling":
            let tool = try mapping(get(detail, "tool", .null))
            let verb = pyStr(get(tool, "verb", .string("")))
            let target = pyStr(get(tool, "target", .string("")))
            if tool.asObject?["reply_preview"] != nil {
                // exit — by the KEY being there, not by its value
                let preview = pyStr(get(tool, "reply_preview", .string("")))
                return "\(prefix) ← \(verb)(\(target))  \(preview)"
            }
            // entry — args summary on the line; `tool.get("args") or {}`, so a
            // falsy args (None, {}, [], 0, "") renders as `{}`.
            let raw = get(tool, "args", .null)
            let args = pyTruthy(raw) ? raw : JSON.object([:])
            return "\(prefix) → \(verb)(\(target))  \(pyJsonDumps(args))"
        default:
            // `streaming` and `done` are silent — handled by other verbs.
            return nil
        }
    }

    static func renderContext(_ payload: JSON) throws(PyError) -> String? {
        let phase = payload["phase"].asString ?? ""
        let detail = try mapping(get(payload, "detail", .null))
        let prefix = linePrefix(source(payload))
        switch phase {
        case "compacted":
            let strat = pyStr(get(detail, "strategy", .string("?")))
            let dropped = pyStr(get(detail, "dropped_messages", .integer(0)))
            return
                "\(prefix) ⟲ context compacted (\(strat), \(dropped) dropped; recall to page back)"
        case "too_small":
            return "\(prefix) ✗ context too small — \(pyStr(get(detail, "hint", .string(""))))"
        default:
            return nil
        }
    }

    // ─── two-phase PTY intro (first contact) ────────────────────

    /// `swift · env=<…> · v<…>? · root=<…> · pid <…>` — the same deployment
    /// context the root reflect carries, rendered for the terminal.
    static func identity(_ kernel: Kernel) -> String {
        let env = ProcessInfo.processInfo.environment["FANTASTIC_ENV"] ?? "host"
        let ver = ProcessInfo.processInfo.environment["FANTASTIC_VERSION"]
        let root = kernel.root?.id.value ?? ROOT_ID
        var parts = ["swift", "env=\(env)"]
        if let ver, !ver.isEmpty { parts.append(ver) }
        parts += ["root=\(root)", "pid \(ProcessInfo.processInfo.processIdentifier)"]
        return parts.joined(separator: " · ")
    }

    static func introBootingLines(_ kernel: Kernel) -> [String] {
        [
            "[fantastic] \(identity(kernel)) — booting…",
            "  one envelope: send(<id>, {\"type\":\"<verb>\", …})"
                + "   ·   kernel = root   ·   full map: reflect readme=true",
            "  PULL  ask → reply        REST POST /<rest>/<id>        ·  this REPL: @<id> <verb> k=v",
            "  PUSH  async stream/emit  WS /<id>/ws : watch{src} · emit{target,payload} · state_subscribe",
            "  REACH one call by id, any unit: "
                + "compute(exec_bridge) · infer(ai) · memory(file_bridge) · shell(exec_bridge pty)",
        ]
    }

    static let bootedLine =
        "[kernel] up — all booted. attach via the map above, or reflect readme=true"

    // ─── stdout ─────────────────────────────────────────────────

    /// THE one write to the terminal — python `_write`: every line this sink
    /// prints goes through `terminalSafe`.
    static func write(_ s: String) {
        FileHandle.standardOutput.write(terminalSafe(s).data(using: .utf8) ?? Data())
    }

    /// `text` without its C0/C1 control characters (and DEL), newline and tab
    /// kept — python's `_CONTROL`: what a terminal INTERPRETS instead of printing
    /// (an OSC title or clipboard write, a cursor move, a line erase). Agent and
    /// model text reaches this sink from callers it does not know.
    /// Judged per scalar, so a `\r\n` pair loses its `\r` and keeps its `\n`.
    static func terminalSafe(_ text: String) -> String {
        var out = String.UnicodeScalarView()
        for u in text.unicodeScalars
        where u == "\n" || u == "\t" || u.properties.generalCategory != .control {
            out.append(u)
        }
        return String(out)
    }

    // ─── help (python's `cli/help.md`, verbatim) ────────────────

    /// `cli/help.md` verbatim — the CLI cheatsheet `fantastic help` / `-h` /
    /// `--help` prints, rstripped with one trailing newline (python
    /// `kernel/modes/help.py:_print_help` → `print(src.read_text().rstrip())`).
    ///
    /// Python reads the file out of the installed `cli` package; swift has no
    /// package resources here, so the bytes live in the bundle that renders
    /// them. 1545 bytes including the trailing newline — the same file.
    public static let helpText = """
        # fantastic — CLI

        A Fantastic kernel is a tree of agents. One primitive:
        `send(target_id, payload) -> reply`. Every agent answers
        `{"type":"reflect"}`.

        ## Invocations

            fantastic                          interactive REPL (tty) and/or
                                               daemon (if a web agent is persisted)
            fantastic <id> <verb> [k=v ...]    one-shot RPC — print JSON, exit
            fantastic reflect [<id>] [k=v]     one-shot: <id> reflect (default 'kernel')
            fantastic --help                   this file

        ## Discover the system

            fantastic reflect                       the live agent tree + bundles
            fantastic reflect readme=true     ...plus the bootstrap readme
            fantastic reflect <id> readme=true  any agent + its readme

        `reflect` is read-only and lock-free — it works whether or not a
        daemon owns the dir. Every other one-shot (`<id> <verb>`) acquires the
        PID lock and is **refused while a daemon is running** —
        go through that daemon's web surface instead (a `web` agent's
        `web_rest` / `web_ws` children; see `fantastic reflect`).

        ## Run a daemon

        There is no `--port` flag. Persist a web agent first, then boot:

            fantastic kernel create_agent parent_id=kernel_state handler_module=web.tools port=8888
            fantastic kernel create_agent parent_id=<web_id> handler_module=web_ws.tools
            fantastic kernel create_agent parent_id=<web_id> handler_module=web_rest.tools
            fantastic                          # rehydrates + serves

        Start with `fantastic reflect readme=true` — it tells you
        everything else.
        """

    // ─── readme (python's `readme.md`, verbatim) ────────────────

    static let readmeText = """
        # cli — stdout renderer
        Renders token/done/say/error events to stdout. Ephemeral — composed per-process when stdin is a tty, never persisted.

        A DUMB SINK for the PTY intro too: it prints what it is told and never inspects the tree. `intro_booting` (kernel→cli, pre-boot) prints the identity + pull/push control-plane map; agents announce their OWN endpoints during boot (e.g. web sends/publishes a `say` with its listening URL); `booted` (kernel→cli, post-boot) is the "all booted" close. Best-effort, tty-only.

        """
}
