// Interactive stdin REPL for an already-booted kernel.
//
// A14.16: a rule-for-rule port of python's `kernel/modes/longrun.py:_repl_loop`
// + `kernel/modes/print.py:_print_result`/`_read_line` +
// `kernel/modes/parse.py:_parse_at`, which are canonical. Composed by
// `DaemonMode.run` when no-args invocation runs in a tty context.
//
// Command grammar (python parity):
//
//   list                          every agent, registration order
//   add <bundle> [k=v ...]        create_agent (boots internally, C79-style;
//                                 python does the same net effect via a
//                                 separate explicit boot send after a
//                                 non-booting `kernel.create()`)
//   delete <id>                   delete_agent
//   @<id>                         send {type:"send", text:""} to <id>
//   @<id> <verb>                  send {type:"<verb>"}             — RPC
//   @<id> <verb> k=v [k=v ...]    send {type:"<verb>", k:v, ...}   — RPC
//   @<id> <multi-word prose>      send {type:"send", text:"<prose>"} — chat
//   exit / quit / Ctrl-D          break (Ctrl-D prints an extra blank line;
//                                 `exit`/`quit` do not — python's `print()`
//                                 lives ONLY in the `except EOFError` branch)
//
// Streaming output (token events from LLM backends) is NOT rendered by the
// REPL itself, matching python: the immediate `send` reply prints (typically
// `{queued: true, stream_id: ...}`) and the prompt returns. Seeing streamed
// tokens needs a `cli` bundle watching the LLM agent — same pattern python
// uses with its `cli` renderer agent (composed by `FantasticCLI.
// composeCliIfTTY`, A14.15), not this loop.

import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// One-slot holder so the blocked reader thread and the cancellation handler
/// hand the same continuation to exactly one resumer.
private final class LineSlot: @unchecked Sendable {
    private let lock = NSLock()
    private var cont: CheckedContinuation<String?, Never>?

    func store(_ c: CheckedContinuation<String?, Never>) {
        lock.lock()
        defer { lock.unlock() }
        cont = c
    }

    func take() -> CheckedContinuation<String?, Never>? {
        lock.lock()
        defer { lock.unlock() }
        let c = cont
        cont = nil
        return c
    }
}

enum REPLMode {

    /// Run the REPL against an already-booted kernel. Returns when the user
    /// hits Ctrl-D / types `exit` / types `quit`. The caller (`DaemonMode`)
    /// owns kernel lifecycle.
    static func run(kernel: Kernel) async {
        while true {
            print("fantastic> ", terminator: "")
            fflush(stdout)
            let read = await readLineAsync()
            // CANCELLED (the daemon's stop leg won the race): return at once,
            // printing NOTHING. Python's `input()` sits on a thread the process
            // abandons at exit, so a signal-driven shutdown produces no REPL
            // output; swift's task group AWAITS this child, so a `readLine()`
            // parked on the tty made `cancelAll()` a no-op — `kernel.shutdown()`
            // never ran and the e2e driver SIGKILLed the daemon (measured:
            // `noargs_pty_fresh`/`noargs_pty_web` exit -9 where python exits 0).
            if Task.isCancelled { return }
            guard let raw = read else {
                // Ctrl-D: python's `except (EOFError, KeyboardInterrupt):
                // print(); break` — the blank line is EOF-only, not shared
                // with the plain `exit`/`quit` break below.
                printLine("")
                break
            }
            let line = pyStrip(raw)
            if line.isEmpty { continue }
            if line == "exit" || line == "quit" { break }

            if line == "list" {
                handleList(kernel: kernel)
                continue
            }
            if line.hasPrefix("add ") {
                await handleAdd(args: String(line.dropFirst(4)), kernel: kernel)
                continue
            }
            if line.hasPrefix("delete ") {
                await handleDelete(id: pyStrip(String(line.dropFirst(7))), kernel: kernel)
                continue
            }
            if line.hasPrefix("@") {
                await handleAt(line: line, kernel: kernel)
                continue
            }
            printLine(unknownCommandLine(line))
        }
    }

    /// Python's `input()` blocks a thread the interpreter abandons at exit.
    /// Swift's `readLine()` blocks whatever executor thread it lands on and no
    /// cancellation can reach it, so the blocking read runs on a global-queue
    /// thread and only the AWAIT is cancellable: on cancellation this returns
    /// `nil` immediately and the parked thread is abandoned, exactly as python
    /// abandons its input thread. The caller tells the two `nil`s apart by
    /// `Task.isCancelled` — a real Ctrl-D is not cancelled and still prints
    /// python's EOF blank line.
    ///
    /// `read` is the blocking read itself — stdin's `readLine()` everywhere but a
    /// test, which needs a read that REALLY blocks: under `swift test` stdin is
    /// at EOF, so `readLine()` returns at once and a test of the abandonable leg
    /// raced it instead of waiting on it (measured: 1 run in ~60 saw the REPL
    /// leg finish first).
    static func readLineAsync(
        _ read: @escaping @Sendable () -> String? = { readLine() }
    ) async -> String? {
        let slot = LineSlot()
        return await withTaskCancellationHandler {
            await withCheckedContinuation { (cont: CheckedContinuation<String?, Never>) in
                slot.store(cont)
                DispatchQueue.global(qos: .userInitiated).async {
                    let line = read()
                    slot.take()?.resume(returning: line)
                }
                // Cancelled before the continuation was stored: honour it now.
                if Task.isCancelled { slot.take()?.resume(returning: nil) }
            }
        } onCancel: {
            slot.take()?.resume(returning: nil)
        }
    }

    /// Python `f"  unknown command: {line!r}  (try: list, add <bundle>,
    /// delete <id>, @<id> ...)"` — `line!r}` is `line`'s python `repr`
    /// (`pyRepr`), never Swift's OWN debug quoting (which single-vs-double
    /// quotes and escapes differently).
    static func unknownCommandLine(_ line: String) -> String {
        "  unknown command: \(pyRepr(line))  (try: list, add <bundle>, delete <id>, @<id> ...)"
    }

    // MARK: - Commands

    /// Python `for a in kernel.list(): ...` — every agent's RAW RECORD in
    /// REGISTRATION order (`kernel.allAgents()`, never the paged
    /// `list_agents` VERB reply, which carries a DIFFERENT shape).
    static func handleList(kernel: Kernel) {
        for agent in kernel.allAgents() {
            let record = agent.record()
            // `a.get('handler_module', '<root>')`: the FALLBACK fires only
            // when the key is ABSENT, never for a present-but-empty value —
            // and this runtime's `handlerModule` has no distinct "present
            // but null" state to lose.
            let hm = record.handlerModule ?? "<root>"
            let tag = pyTruthy(record.meta["singleton"] ?? .null) ? " (singleton)" : ""
            printLine("  \(record.id)\(tag)  →  \(hm)")
        }
    }

    /// Python `add <bundle> [k=v ...]`: resolve the bundle NAME to its
    /// `handler_module` first (`  unknown bundle '<bundle>'` if it does not
    /// resolve), THEN create + print via the shared `_print_result` rule.
    static func handleAdd(args raw: String, kernel: Kernel) async {
        let parts = splitArgs(raw)
        guard let bundle = parts.first else {
            printLine("  usage: add <bundle> [k=v ...]")
            return
        }
        // Swift has no runtime package-entry-point registry to resolve a
        // short bundle NAME the way python's `_find_bundle_module` does
        // (`importlib.metadata.entry_points`); every bundle this runtime
        // ships registers itself as `<name>.tools` (`FantasticKernelStartup/
        // Startup.swift`), so the convention IS the resolution — checked
        // for EXISTENCE in the live registry rather than assumed.
        let handlerModule = bundle.contains(".") ? bundle : "\(bundle).tools"
        guard kernel.bundles.get(handlerModule) != nil else {
            printLine("  unknown bundle \(pyRepr(bundle))")
            return
        }
        var payload: OrderedDictionary<String, JSON> = [
            "type": .string("create_agent"),
            "parent_id": .string(kernel.root?.id.value ?? ROOT_ID),
            "handler_module": .string(handlerModule),
        ]
        for p in parts.dropFirst() {
            guard let eq = p.firstIndex(of: "=") else { continue }
            payload[String(p[..<eq])] = cliCoerce(String(p[p.index(after: eq)...]))
        }
        // ONE call: this runtime's `create_agent` verb boots the child
        // INTERNALLY and returns the PRE-BOOT record snapshot (C79-era
        // design, `SystemVerbs.swift:429-459`) — the same net result and
        // the same printed shape as python's separate `kernel.create()` +
        // explicit boot send, whose boot reply is discarded anyway.
        let reply = await kernel.send([Item(AgentId("kernel"), .object(payload))])[0].result
        printResult(reply)
    }

    /// Python `delete <id>`: no usage guard for an empty id either — an
    /// empty string is just an id `kernel.delete` does not know, and
    /// `_print_result` renders THAT refusal like any other.
    static func handleDelete(id: String, kernel: Kernel) async {
        let reply = await kernel.send([
            Item(
                AgentId("kernel"),
                .object(["type": .string("delete_agent"), "id": .string(id)]))
        ])[0].result
        printResult(reply)
    }

    /// `@<id> ...` — python `_parse_at` + the REPL's own usage guard when it
    /// answers no target.
    private static func handleAt(line: String, kernel: Kernel) async {
        guard let cmd = parseAtCommand(line) else {
            printLine("  usage: @<id> <text> | @<id> <verb> k=v ...")
            return
        }
        let reply = await kernel.send([Item(AgentId(cmd.target), cmd.payload)])[0].result
        printResult(reply)
    }

    /// Pure parse of a REPL `@`-line into a (target, payload) pair — no
    /// kernel side effects, so it's unit-testable. `nil` for an empty `@`
    /// (python's `_parse_at("@")` returns `("", {})`; the CALLER is what
    /// turns an empty target into the usage line — kept as `nil` here so a
    /// twin can tell "no target" apart from "target that happens to be the
    /// empty string" the same way python's `if not target:` does, since
    /// both collapse to falsy there).
    ///
    ///   `@id`               → {type:"send", text:""}
    ///   `@id <verb>`        → {type:"<verb>"}                 (single bare token)
    ///   `@id <verb> k=v...` → {type:"<verb>", k:v, ...}
    ///   `@id k=v...`        → {type:"send", k:v, ...}         (leading kv ⇒ verb=send)
    ///   `@id some words`    → {type:"send", text:"some words"} (whitespace preserved)
    static func parseAtCommand(_ line: String) -> (target: String, payload: JSON)? {
        // python `rest = line[1:].strip()`.
        let rest = pyStrip(String(line.dropFirst()))
        if rest.isEmpty { return nil }
        let parts = splitArgs(rest)
        let target = parts[0]
        let argTokens = Array(parts.dropFirst())

        if argTokens.isEmpty {
            return (target, .object(["type": .string("send"), "text": .string("")]))
        }

        if argTokens.count == 1 && !argTokens[0].contains("=") {
            return (target, .object(["type": .string(argTokens[0])]))
        }

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
            var payload: OrderedDictionary<String, JSON> = ["type": .string(verb)]
            for t in kvTokens {
                guard let eq = t.firstIndex(of: "=") else { continue }
                payload[String(t[..<eq])] = cliCoerce(String(t[t.index(after: eq)...]))
            }
            return (target, .object(payload))
        }

        // Multi-token, no `=` anywhere → chat send with the literal text
        // after the target — reconstructed from `rest` (STRIPPED), python's
        // `rest[len(target):].strip()`, not the raw unstripped line (which
        // could misalign when `@` is followed by extra whitespace before
        // the target).
        let text = pyStrip(String(rest.dropFirst(target.count)))
        return (target, .object(["type": .string("send"), "text": .string(text)]))
    }

    /// Python `_print_result`: `None` silent; a dict carrying the KEY
    /// `error` (any value, present-ness only — never truthiness) prints
    /// `  error: <str(error)>`; a dict carrying BOTH `id` and
    /// `handler_module` keys prints `  created <id>`; else the WHOLE reply
    /// as `  ` + `json.dumps(result, indent=2, default=str)` (ensure_ascii
    /// default true — `FantasticCLI.replyJSON`'s exact bytes). `nil` = print
    /// nothing (python's `if result is None: return`).
    ///
    /// Returns the line rather than printing it directly so a twin can pin
    /// the text without capturing stdout.
    static func printResultLine(_ result: JSON) -> String? {
        if case .null = result { return nil }
        if case .object(let o) = result {
            if let err = o["error"] {
                return "  error: \(pyStr(err))"
            }
            if o["id"] != nil && o["handler_module"] != nil {
                return "  created \(pyStr(o["id"]!))"
            }
        }
        return "  " + FantasticCLI.replyJSON(result)
    }

    private static func printResult(_ result: JSON) {
        guard let line = printResultLine(result) else { return }
        printLine(line)
    }

    // MARK: - Parsing helpers

    /// Split a line on whitespace, honoring double-quoted segments — a
    /// subset of `shlex.split` covering what `add`/`delete`/`@` exercise.
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

    private static func printLine(_ s: String) {
        RuntimeLog.out(s)
    }
}
