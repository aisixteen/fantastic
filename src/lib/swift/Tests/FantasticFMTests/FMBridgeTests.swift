// The C ABI, driven through its own `@_cdecl` functions with C callbacks and a
// raw `ctx` — exactly the calls the rust bundle makes — against a fake engine.
//
// SERIALIZED: the bridge's engine is process-wide, so every test that installs
// one runs alone (the opt-in live test, `FMLiveTests.swift`, extends this suite
// for the same reason).

import FantasticFMCore
import Foundation
import Testing

@testable import FantasticFMBridge

/// What the callbacks saw for one handle — reached through `ctx`.
final class Collector: @unchecked Sendable {
    private let lock = NSLock()
    private var _bytes = Data()
    private var _chunks = 0
    private var _done = 0
    private var _error: String?
    private var _promptTokens: Int64 = -2
    private var _afterDone = 0

    func chunk(_ p: UnsafePointer<UInt8>?, _ n: UInt) {
        lock.withLock {
            if _done > 0 { _afterDone += 1 }
            if let p { _bytes.append(p, count: Int(n)) }
            _chunks += 1
        }
    }

    func done(_ e: UnsafePointer<CChar>?, _ tokens: Int64) {
        lock.withLock {
            _done += 1
            _error = e.map { String(cString: $0) }
            _promptTokens = tokens
        }
    }

    var text: String { lock.withLock { String(decoding: _bytes, as: UTF8.self) } }
    var chunks: Int { lock.withLock { _chunks } }
    var doneCount: Int { lock.withLock { _done } }
    var error: String? { lock.withLock { _error } }
    var promptTokens: Int64 { lock.withLock { _promptTokens } }
    var callsAfterDone: Int { lock.withLock { _afterDone } }

    var ctx: UnsafeMutableRawPointer { Unmanaged.passUnretained(self).toOpaque() }
}

/// The C callbacks — no captures, the collector rides in `ctx`.
nonisolated(unsafe) let onChunk: FMChunkCallback = { ctx, bytes, len in
    Unmanaged<Collector>.fromOpaque(ctx!).takeUnretainedValue().chunk(bytes, len)
}
nonisolated(unsafe) let onDone: FMDoneCallback = { ctx, error, tokens in
    Unmanaged<Collector>.fromOpaque(ctx!).takeUnretainedValue().done(error, tokens)
}

/// `fm_stream` with a Swift string as the borrowed `request_json`.
func stream(_ json: String?, _ c: Collector) -> UInt64 {
    guard let json else { return fm_stream(nil, c.ctx, onChunk, onDone) }
    return json.withCString { fm_stream($0, c.ctx, onChunk, onDone) }
}

let TINY = #"{"instructions":"","turns":[],"prompt":"hi"}"#

@Suite("FMBridge C ABI", .serialized)
struct FMBridgeTests {

    /// Install `engine` for one test; the system engine is put back after.
    func with(_ engine: any FMEngine, _ body: () async throws -> Void) async rethrows {
        FMBridge.engine = engine
        defer { FMBridge.engine = SystemFMEngine() }
        try await body()
    }

    @Test func availabilityIsAHeapJsonStringTheCallerFrees() async {
        await with(FakeEngine(availability: .unavailable(reason: FMReason.appleIntelligenceNotEnabled))) {
            let p = fm_availability()
            #expect(p != nil)
            #expect(
                String(cString: p!)
                    == "{\"available\":false,\"reason\":\(jsonString(FMReason.appleIntelligenceNotEnabled))}")
            fm_free(p)
            fm_free(nil)  // a no-op
        }
        await with(FakeEngine()) {
            let p = fm_availability()
            #expect(String(cString: p!) == #"{"available":true,"reason":null}"#)
            fm_free(p)
        }
    }

    @Test func theContextSizeIsTheEngines() async {
        await with(FakeEngine(contextSize: 4096)) { #expect(fm_context_size() == 4096) }
        await with(FakeEngine(contextSize: 8192)) { #expect(fm_context_size() == 8192) }
    }

    @Test func aStreamDeliversDeltasThenDoneOnceWithThePromptCount() async {
        let engine = FakeEngine(script: [[.text("Hel"), .text("lo — "), .text("wörld"), .promptTokens(88)]])
        await with(engine) {
            let c = Collector()
            let h = stream(
                #"{"instructions":"sys","turns":[{"role":"user","content":"a"},{"role":"assistant","content":"b"}],"prompt":"hi","temperature":0.5,"max_tokens":7}"#,
                c)
            #expect(h >= 1)
            #expect(await eventually { c.doneCount == 1 })
            #expect(c.text == "Hello — wörld")
            #expect(c.chunks == 3)
            #expect(c.error == nil)
            #expect(c.promptTokens == 88)
            // the request reached the engine as sent
            #expect(
                engine.requests
                    == [
                        FMRequest(
                            instructions: "sys",
                            turns: [.init(role: "user", content: "a"), .init(role: "assistant", content: "b")],
                            prompt: "hi", temperature: 0.5, maxTokens: 7)
                    ])
            #expect(await eventually { FMBridge.live == 0 }, "a finished handle is forgotten")
            try? await Task.sleep(nanoseconds: 20_000_000)
            #expect(c.doneCount == 1 && c.callsAfterDone == 0)
        }
    }

    @Test func noPromptCountIsMinusOne() async {
        await with(FakeEngine(script: [[.text("x")]])) {
            let c = Collector()
            _ = stream(TINY, c)
            #expect(await eventually { c.doneCount == 1 })
            #expect(c.error == nil && c.promptTokens == -1)
        }
    }

    @Test func handlesAreDistinctAndNonZero() async {
        await with(FakeEngine()) {
            let a = Collector()
            let b = Collector()
            let ha = stream(TINY, a)
            let hb = stream(TINY, b)
            #expect(ha >= 1 && hb >= 1 && ha != hb)
            #expect(await eventually { a.doneCount == 1 && b.doneCount == 1 })
        }
    }

    @Test func aNullCallbackIsRefusedWithZeroAndNothingIsCalled() async {
        await with(FakeEngine()) {
            let c = Collector()
            #expect(TINY.withCString { fm_stream($0, c.ctx, nil, onDone) } == 0)
            #expect(TINY.withCString { fm_stream($0, c.ctx, onChunk, nil) } == 0)
            try? await Task.sleep(nanoseconds: 20_000_000)
            #expect(c.doneCount == 0 && c.chunks == 0)
        }
    }

    @Test func aNullOrMalformedRequestFailsThroughOnDone() async {
        await with(FakeEngine()) {
            let a = Collector()
            #expect(stream(nil, a) >= 1)
            #expect(await eventually { a.doneCount == 1 })
            #expect(a.error == "request_json is NULL")

            let b = Collector()
            _ = stream(#"{"turns":[],"prompt":"p"}"#, b)
            #expect(await eventually { b.doneCount == 1 })
            #expect(b.error == "request_json: `instructions` is missing")
            #expect(b.chunks == 0)
        }
    }

    @Test func anUnavailableModelIsRefusedNamingTheReason() async {
        await with(FakeEngine(availability: .unavailable(reason: FMReason.deviceNotEligible))) {
            let c = Collector()
            _ = stream(TINY, c)
            #expect(await eventually { c.doneCount == 1 })
            #expect(c.error == "the on-device model is unavailable: \(FMReason.deviceNotEligible)")
            #expect(c.chunks == 0)
        }
    }

    @Test func anInvalidTurnIsRefusedInFMCoresWords() async {
        await with(FakeEngine()) {
            let c = Collector()
            _ = stream(#"{"instructions":"","turns":[{"role":"tool","content":"x"}],"prompt":"p"}"#, c)
            #expect(await eventually { c.doneCount == 1 })
            #expect(
                c.error
                    == "turn 0 has role \"tool\" — a transcript turn is user or assistant (system "
                    + "text goes in instructions)")
        }
    }

    @Test func aGenerationErrorAfterTextStillEndsInOneDone() async {
        await with(FakeEngine(script: [[.text("par")]], failure: FMError("the on-device model failed: x"))) {
            let c = Collector()
            _ = stream(TINY, c)
            #expect(await eventually { c.doneCount == 1 })
            #expect(c.text == "par")
            #expect(c.error == "the on-device model failed: x")
        }
    }

    @Test func cancelEndsTheStreamWithCancelledExactlyOnce() async {
        await with(FakeEngine(script: [[.text("first")]], hang: true)) {
            let c = Collector()
            let h = stream(TINY, c)
            #expect(await eventually { c.chunks == 1 })
            #expect(c.doneCount == 0, "still generating")
            fm_cancel(h)
            #expect(await eventually { c.doneCount == 1 })
            #expect(c.error == FM_CANCELLED)
            #expect(c.text == "first")
            #expect(await eventually { FMBridge.live == 0 })
            // a finished or unknown handle is a no-op
            fm_cancel(h)
            fm_cancel(987_654_321)
            try? await Task.sleep(nanoseconds: 20_000_000)
            #expect(c.doneCount == 1)
        }
    }
}
