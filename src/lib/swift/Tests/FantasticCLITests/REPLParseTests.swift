// CLI smoke tests — REPL `@`-command parsing + k=v coercion.
//
// Daemon mode (no-args boot + block/REPL) is exercised end-to-end by
// the repo-root `integration_tests/` Python orchestrator, which
// spawns this binary in daemon mode. These unit tests cover the pure
// parsing logic that has no other coverage: the strict Python-REPL
// `@`-command grammar and the CLI's `k=v` value coercion.

import FantasticJSON
import FantasticKernel
import Foundation
import Testing

@testable import Fantastic

@Suite("REPL @-command parsing")
struct REPLParseTests {
    @Test func bareTargetSendsEmptyChat() {
        let cmd = REPLMode.parseAtCommand("@chat")
        #expect(cmd?.target == "chat")
        #expect(cmd?.payload["type"].asString == "send")
        #expect(cmd?.payload["text"].asString == "")
    }

    @Test func singleBareTokenIsVerb() {
        let cmd = REPLMode.parseAtCommand("@chat reflect")
        #expect(cmd?.target == "chat")
        #expect(cmd?.payload["type"].asString == "reflect")
        // No extra keys beyond type.
        if case .object(let obj) = cmd!.payload {
            #expect(obj.count == 1)
        } else {
            Issue.record("payload not an object")
        }
    }

    @Test func verbWithKVArgs() {
        let cmd = REPLMode.parseAtCommand("@canvas add_agent x=10 y=20")
        #expect(cmd?.target == "canvas")
        #expect(cmd?.payload["type"].asString == "add_agent")
        #expect(cmd?.payload["x"].asInt == 10)
        #expect(cmd?.payload["y"].asInt == 20)
    }

    @Test func leadingKVDefaultsToSendVerb() {
        let cmd = REPLMode.parseAtCommand("@chat text=hi mode=fast")
        #expect(cmd?.target == "chat")
        #expect(cmd?.payload["type"].asString == "send")
        #expect(cmd?.payload["text"].asString == "hi")
        #expect(cmd?.payload["mode"].asString == "fast")
    }

    @Test func multiWordIsChatTextWhitespacePreserved() {
        let cmd = REPLMode.parseAtCommand("@chat hello there  world")
        #expect(cmd?.target == "chat")
        #expect(cmd?.payload["type"].asString == "send")
        // Internal double-space preserved (reconstructed from the raw
        // line, not collapsed by the tokenizer).
        #expect(cmd?.payload["text"].asString == "hello there  world")
    }

    @Test func quotedKVValueKeepsSpaces() {
        let cmd = REPLMode.parseAtCommand("@chat send text=\"hi there\"")
        #expect(cmd?.target == "chat")
        #expect(cmd?.payload["type"].asString == "send")
        #expect(cmd?.payload["text"].asString == "hi there")
    }

    @Test func emptyAtReturnsNil() {
        #expect(REPLMode.parseAtCommand("@") == nil)
        #expect(REPLMode.parseAtCommand("@   ") == nil)
    }

    @Test func kvCoercionTypes() {
        let cmd = REPLMode.parseAtCommand("@a v boolt=true boolf=false n=42 s=hello")
        #expect(cmd?.payload["boolt"].asBool == true)
        #expect(cmd?.payload["boolf"].asBool == false)
        #expect(cmd?.payload["n"].asInt == 42)
        #expect(cmd?.payload["s"].asString == "hello")
    }

    /// A14.16: leading whitespace between `@` and the target must not
    /// misalign the reconstructed chat text — python's `rest = line[1:]
    /// .strip()` drops it before `target` is even found, so the trailing
    /// slice (`rest[len(target):]`) starts right after `target`, not after
    /// wherever `target` happened to appear in the UNSTRIPPED line.
    @Test func leadingWhitespaceAfterAtDoesNotMisalignChatText() {
        let cmd = REPLMode.parseAtCommand("@  chat  hello there")
        #expect(cmd?.target == "chat")
        #expect(cmd?.payload["type"].asString == "send")
        #expect(cmd?.payload["text"].asString == "hello there")
    }
}

// A14.16: `_print_result` — python `kernel/modes/print.py`.
@Suite("REPL _print_result")
struct REPLPrintResultTests {
    @Test func printResultMatchesPython() {
        // `None` → nothing printed.
        #expect(REPLMode.printResultLine(.null) == nil)

        // `error` triggers on KEY PRESENCE, never truthiness — a falsy
        // `error` (0, "", []) still renders, and the value renders via
        // python's bare `str()`, not a repr (no quotes on a string).
        #expect(
            REPLMode.printResultLine(.object(["error": .string("boom")])) == "  error: boom")
        #expect(REPLMode.printResultLine(.object(["error": .integer(0)])) == "  error: 0")
        #expect(
            REPLMode.printResultLine(.object(["error": .array([.string("a")])]))
                == "  error: ['a']")

        // BOTH `id` and `handler_module` keys present → `created <id>`,
        // checked BEFORE the generic JSON fallback and independent of
        // `error` (which wins when both are present, since python checks
        // `error` first).
        #expect(
            REPLMode.printResultLine(
                .object(["id": .string("web_ab12cd"), "handler_module": .string("web.tools")]))
                == "  created web_ab12cd")
        #expect(
            REPLMode.printResultLine(
                .object([
                    "error": .string("exists"), "id": .string("x"),
                    "handler_module": .string("x.tools"),
                ])) == "  error: exists")

        // Only ONE of `id`/`handler_module` present → the generic fallback,
        // `  ` + ensure_ascii indent-2 JSON.
        #expect(
            REPLMode.printResultLine(.object(["id": .string("x")]))
                == "  {\n  \"id\": \"x\"\n}")

        // Neither `error` nor `id`+`handler_module` → the same fallback —
        // e.g. a bare `{ok:true}` reply, or a scalar/array result.
        #expect(
            REPLMode.printResultLine(.object(["ok": .bool(true)])) == "  {\n  \"ok\": true\n}")
        #expect(
            REPLMode.printResultLine(.array([.integer(1), .integer(2)])) == "  [\n  1,\n  2\n]")
    }

    /// A14.16: python's `unknown command` line uses `{line!r}` — python
    /// `repr`, single-quoted, never Swift's own `debugDescription`
    /// (double-quoted, different escapes).
    @Test func unknownCommandUsesPythonRepr() {
        #expect(
            REPLMode.unknownCommandLine("bogus")
                == "  unknown command: 'bogus'  (try: list, add <bundle>, delete <id>, @<id> ...)")
        // A string containing a single quote flips python's repr to double
        // quotes — the exact case Swift's `debugDescription` would get
        // wrong (it always uses double quotes and different escaping).
        #expect(
            REPLMode.unknownCommandLine("it's broken")
                == #"  unknown command: "it's broken"  (try: list, add <bundle>, delete <id>, @<id> ...)"#
        )
    }
}

private struct REPLTestNoop: AgentBundle {
    let name = "noop"
    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? { .null }
}

/// A14.16: `list` / `add` / `delete` end to end — a real (in-memory) kernel,
/// asserting on the STDOUT lines each command produces. `.serialized`:
/// `RuntimeLog`'s capture sinks are a shared array every ACTIVE capture
/// appends to (`RuntimeLog.swift`'s own doc: "two suites can be capturing at
/// the same time"), so two of these tests racing would each see the OTHER's
/// lines too.
@Suite("REPL list/add/delete", .serialized)
struct REPLCommandTests {
    private func makeReplKernel() -> Kernel {
        let reg = BundleRegistry()
        reg.register("noop.tools", REPLTestNoop())
        let kernel = Kernel(storage: .inMemory, bundles: reg)
        let root = Agent(id: AgentId(ROOT_ID), handlerModule: "noop.tools", parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        return kernel
    }

    @Test func listShowsEveryAgentInRegistrationOrderWithSingletonTag() async {
        let kernel = makeReplKernel()
        _ = kernel.composeEphemeralChild(
            id: AgentId("cli"), handlerModule: "noop.tools", parent: kernel.root!,
            meta: ["singleton": .bool(true)])
        let lines = await RuntimeLog.captureOut {
            REPLMode.handleList(kernel: kernel)
        }
        #expect(
            lines == [
                "  \(ROOT_ID)  →  noop.tools",
                "  cli (singleton)  →  noop.tools",
            ], "\(lines)")
    }

    @Test func addRefusesAnUnknownBundle() async {
        let kernel = makeReplKernel()
        let lines = await RuntimeLog.captureOut {
            await REPLMode.handleAdd(args: "no_such_bundle", kernel: kernel)
        }
        #expect(lines == ["  unknown bundle 'no_such_bundle'"], "\(lines)")
        #expect(kernel.allAgents().count == 1, "nothing created")
    }

    @Test func addCreatesAndPrintsTheCreatedLine() async {
        let kernel = makeReplKernel()
        let lines = await RuntimeLog.captureOut {
            await REPLMode.handleAdd(args: "noop id=extra", kernel: kernel)
        }
        #expect(lines == ["  created extra"], "\(lines)")
        #expect(kernel.agent(AgentId("extra")) != nil)
    }

    @Test func deleteRemovesTheAgent() async {
        let kernel = makeReplKernel()
        await REPLMode.handleAdd(args: "noop id=doomed", kernel: kernel)
        #expect(kernel.agent(AgentId("doomed")) != nil)
        _ = await RuntimeLog.captureOut {
            await REPLMode.handleDelete(id: "doomed", kernel: kernel)
        }
        #expect(kernel.agent(AgentId("doomed")) == nil)
    }

    /// No usage guard for an empty id — python has none either; an empty
    /// string is just an unknown agent id, refused like any other.
    @Test func deleteUnknownIdPrintsAnErrorLine() async {
        let kernel = makeReplKernel()
        let lines = await RuntimeLog.captureOut {
            await REPLMode.handleDelete(id: "ghost", kernel: kernel)
        }
        #expect(lines.count == 1)
        #expect(lines[0].hasPrefix("  error: "), "\(lines)")
    }
}

// A14.10 — ONE `k=v` rule for every CLI surface (one-shot, `reflect`, the
// REPL's `@id verb k=v` and `add k=v`): python `kernel/modes/parse.py:_coerce`.
@Suite("REPL stdin cancellation")
struct REPLCancelTests {

    /// The daemon races the REPL leg against the stop signal in a task group,
    /// and a task group AWAITS every child before it returns. `readLine()` is
    /// an uninterruptible blocking read, so a SIGTERM under a PTY cancelled a
    /// child that never came back: `kernel.shutdown()` never ran, the teardown
    /// lines never printed, and the e2e driver had to SIGKILL the daemon
    /// (measured 2026-09-21 — `cli/noargs_pty_fresh` and `cli/noargs_pty_web`
    /// exit `-9` where python exits `0`; reproduced standalone at 15.6 s to
    /// SIGKILL, 0.6 s after the fix).
    ///
    /// Python's `input()` blocks a thread the interpreter abandons at exit, so
    /// the leg has to be abandonable here too: the blocking read is parked on a
    /// global-queue thread and only the AWAIT is cancellable.
    @Test func cancelledReadReturnsWithoutWaitingForStdin() async {
        let t = Task { await REPLMode.readLineAsync() }
        t.cancel()
        // The whole point: this `await` completes. Before the fix it waited on
        // a `readLine()` no cancellation could reach.
        let line = await t.value
        #expect(line == nil)
        #expect(t.isCancelled)
    }

    /// The daemon's shape, reduced: a task group holding the REPL leg plus a
    /// stop leg that wins. `withTaskGroup` does not return until the REPL leg
    /// does, so this test HANGS on the old code and passes on the new.
    @Test func groupHoldingTheReplLegCompletesWhenTheStopLegWins() async {
        let done = await withTaskGroup(of: String?.self) { group -> String? in
            group.addTask {
                _ = await REPLMode.readLineAsync()
                return nil
            }
            group.addTask { "SIGTERM" }
            let first = await group.next()
            group.cancelAll()
            return first ?? nil
        }
        #expect(done == "SIGTERM")
    }
}

@Suite("CLI k=v coercion")
struct ParseKVTests {
    @Test func coercesScalarTypes() {
        #expect(cliCoerce("true").asBool == true)
        #expect(cliCoerce("false").asBool == false)
        // The bool test is `v.lower()` — case-insensitive.
        #expect(cliCoerce("TRUE").asBool == true)
        #expect(cliCoerce("FALSE").asBool == false)
        #expect(cliCoerce("12345").asInt == 12345)
        #expect(cliCoerce("bridge").asString == "bridge")
        // A decimal is a float, and so is an exponent — python `float()`.
        #expect(cliCoerce("1.5").asDouble == 1.5)
        #expect(cliCoerce("1e3").asDouble == 1000.0)
    }

    @Test func coercesJsonObjectAndArray() {
        // The OPERATOR's extra branch: a `{…}` / `[…]` literal that parses.
        #expect(cliCoerce(#"{"a": 1}"#)["a"].asInt == 1)
        #expect(cliCoerce("[1, 2]").asArray?.count == 2)
        // A literal that does NOT parse stays the string as written.
        #expect(cliCoerce("{nope").asString == "{nope")
        #expect(cliCoerce("{a: 1}").asString == "{a: 1}")
    }

    @Test func duplicateKeyLastWins() {
        // python `{"type": verb, **_parse_kv(...)}`: a repeat keeps its FIRST
        // position and takes the LAST value. `uniqueKeysWithValues` used to
        // trap here with `Fatal error: Duplicate key`.
        let p = cliPayload(verb: "list_agents", kvTokens: ["limit=1", "limit=2"])
        #expect(p["limit"].asInt == 2)
        #expect(p.asObject?.count == 2)
        // `type=` overrides the verb in place — still the first key.
        let t = cliPayload(verb: "reflect", kvTokens: ["type=list_agents"])
        #expect(t["type"].asString == "list_agents")
        #expect(t.asObject?.keys.first == "type")
        #expect(t.asObject?.count == 1)
        // A token with no `=` is skipped, not an error.
        #expect(cliPayload(verb: "reflect", kvTokens: ["junk"]).asObject?.count == 1)
    }
}
