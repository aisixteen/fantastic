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

@Suite("CLI k=v coercion")
struct ParseKVTests {
    @Test func coercesScalarTypes() {
        #expect(parseKV("true").asBool == true)
        #expect(parseKV("false").asBool == false)
        #expect(parseKV("12345").asInt == 12345)
        #expect(parseKV("bridge").asString == "bridge")
        // Non-integer numeric strings stay strings (parseKV only
        // coerces whole Int64s + bools).
        #expect(parseKV("1.5").asString == "1.5")
    }
}
