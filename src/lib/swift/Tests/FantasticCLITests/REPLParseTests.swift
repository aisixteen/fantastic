// CLI smoke tests — REPL `@`-command parsing + k=v coercion.
//
// Daemon mode (no-args boot + block/REPL) is exercised end-to-end by
// the repo-root `integration_tests/` Python orchestrator, which
// spawns this binary in daemon mode. These unit tests cover the pure
// parsing logic that has no other coverage: the strict Python-REPL
// `@`-command grammar and the CLI's `k=v` value coercion.

import FantasticJSON
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
        if case let .object(obj) = cmd!.payload {
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
}

// A14.10 — ONE `k=v` rule for every CLI surface (one-shot, `reflect`, the
// REPL's `@id verb k=v` and `add k=v`): python `kernel/modes/parse.py:_coerce`.
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
