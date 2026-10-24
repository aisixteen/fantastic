// FantasticFMCore's pure parts — the snapshot→delta diff, the request contract
// and its JSON, the availability JSON, the error words, and the transcript the
// real engine builds. Nothing here calls the model.

import Foundation
import Testing

@testable import FantasticFMCore

#if canImport(FoundationModels) && os(macOS)
    import FoundationModels
#endif

@Suite("FMCore")
struct FMCoreTests {

    // MARK: deltas

    @Test func cumulativeSnapshotsBecomeDeltas() throws {
        var d = FMDeltas()
        #expect(try d.next("") == nil)
        #expect(try d.next("Hel") == "Hel")
        #expect(try d.next("Hel") == nil, "a repeated snapshot carries nothing new")
        #expect(try d.next("Hello") == "lo")
        #expect(try d.next("Hello, world") == ", world")
    }

    @Test func aByteThatJoinsTheLastGraphemeIsStillADelta() throws {
        // `e` then a combining acute: as Characters "é" does not start with "e";
        // as UTF-8 it extends it, and the delta is the accent alone.
        var d = FMDeltas()
        #expect(try d.next("caf" + "e") == "cafe")
        #expect(try d.next("cafe\u{301}") == "\u{301}")
    }

    @Test func aSnapshotThatRewritesSentTextIsAnError() {
        var d = FMDeltas()
        _ = try? d.next("Hello")
        #expect(throws: FMError.self) { try d.next("Help") }
        do {
            _ = try d.next("Jello")
            Issue.record("a rewrite must throw")
        } catch {
            #expect(
                error.message
                    == "the on-device stream rewrote text it had already streamed (5 bytes sent, "
                    + "the new snapshot does not extend them)")
        }
    }

    // MARK: the request

    @Test func onlyUserAndAssistantTurnsAreTranscriptTurns() {
        let bad = FMRequest(
            instructions: "", turns: [.init(role: "user", content: "a"), .init(role: "system", content: "s")],
            prompt: "p")
        #expect(throws: FMError.self) { try bad.validate() }
        do { try bad.validate() } catch {
            #expect(
                error.message
                    == "turn 1 has role \"system\" — a transcript turn is user or assistant "
                    + "(system text goes in instructions)")
        }
        #expect(throws: Never.self) {
            try FMRequest(
                instructions: "i",
                turns: [.init(role: "user", content: "a"), .init(role: "assistant", content: "b")],
                prompt: "p", temperature: 0, maxTokens: 1
            ).validate()
        }
    }

    @Test func optionsOutOfRangeAreRefusedNamingTheValue() {
        func message(_ r: FMRequest) -> String? {
            do { try r.validate() } catch { return error.message }
            return nil
        }
        #expect(
            message(FMRequest(instructions: "", turns: [], prompt: "p", temperature: -0.5))
                == "temperature must be a number >= 0, got -0.5")
        #expect(
            message(FMRequest(instructions: "", turns: [], prompt: "p", temperature: .nan))
                == "temperature must be a number >= 0, got nan")
        #expect(
            message(FMRequest(instructions: "", turns: [], prompt: "p", maxTokens: 0))
                == "max_tokens must be a positive int, got 0")
    }

    @Test func theRequestJsonParses() throws {
        let r = try FMRequest(
            json: Data(
                #"{"instructions":"be brief","turns":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"}],"prompt":"again","temperature":0.2,"max_tokens":64,"extra":1}"#
                    .utf8))
        #expect(
            r
                == FMRequest(
                    instructions: "be brief",
                    turns: [.init(role: "user", content: "hi"), .init(role: "assistant", content: "hello")],
                    prompt: "again", temperature: 0.2, maxTokens: 64))
        // null / absent options are the model's own
        let bare = try FMRequest(
            json: Data(#"{"instructions":"","turns":[],"prompt":"p","temperature":null}"#.utf8))
        #expect(bare.temperature == nil && bare.maxTokens == nil)
    }

    @Test func aMalformedRequestJsonNamesThePathThatFailed() {
        func message(_ json: String) -> String? {
            do { _ = try FMRequest(json: Data(json.utf8)) } catch { return error.message }
            return nil
        }
        #expect(message(#"{"turns":[],"prompt":"p"}"#) == "request_json: `instructions` is missing")
        #expect(
            message(#"{"instructions":"","turns":[{"role":"user"}],"prompt":"p"}"#)
                == "request_json: `turns[0].content` is missing")
        #expect(
            message(#"{"instructions":"","turns":[],"prompt":5}"#)
                == "request_json: `prompt` is not a string")
        #expect(
            message(#"{"instructions":"","turns":[],"prompt":"p","max_tokens":"9"}"#)
                == "request_json: `max_tokens` is not an int")
        #expect(
            message(#"{"instructions":"","turns":"x","prompt":"p"}"#)
                == "request_json: `turns` is not a list")
        #expect(
            message(#"{"instructions":null,"turns":[],"prompt":"p"}"#)
                == "request_json: `instructions` is null, expected a string")
        #expect(message("[1]") == "request_json: the document is not an object")
        #expect(message("not json")?.hasPrefix("request_json: ") == true)
    }

    // MARK: availability, errors, JSON

    @Test func availabilityIsCompactJsonInKeyOrder() {
        #expect(FMAvailability.available.json == #"{"available":true,"reason":null}"#)
        #expect(
            FMAvailability.unavailable(reason: FMReason.modelNotReady).json
                == #"{"available":false,"reason":"model not ready — the on-device model is still downloading or preparing; retry once it is ready"}"#
        )
        #expect(FMAvailability.available.isAvailable && FMAvailability.available.reason == nil)
    }

    @Test func theReasonSentencesNameTheReason() {
        #expect(FMReason.deviceNotEligible.hasPrefix("device not eligible"))
        #expect(FMReason.appleIntelligenceNotEnabled.hasPrefix("Apple Intelligence is not enabled"))
        #expect(FMReason.modelNotReady.hasPrefix("model not ready"))
        #expect(FMReason.platform("Linux") == "platform not macOS 26+ — this is Linux")
        #expect(
            fmUnavailableMessage(FMReason.platform("Linux"))
                == "the on-device model is unavailable: platform not macOS 26+ — this is Linux")
    }

    @Test func thePreflightRefusesAnUnavailableModelBeforeTheRequest() {
        // unavailable AND invalid: the reason is what the caller reads
        let bad = FMRequest(instructions: "", turns: [.init(role: "tool", content: "")], prompt: "p")
        do {
            try fmPreflight(.unavailable(reason: FMReason.deviceNotEligible), bad)
            Issue.record("must refuse")
        } catch {
            #expect(error.message == fmUnavailableMessage(FMReason.deviceNotEligible))
        }
        #expect(throws: FMError.self) { try fmPreflight(.available, bad) }
    }

    @Test func aFrameworkErrorIsSaidInItsOwnWordsOnce() {
        struct Plain: Error, CustomStringConvertible { var description: String { "boom" } }
        #expect(fmMessage(FMError("as is")) == "as is")
        let m = fmMessage(Plain())
        #expect(m.hasPrefix("the on-device model failed: "), "\(m)")
        #expect(m.contains("boom"), "\(m)")
    }

    @Test func jsonStringEscapesExactlyWhatJsonRequires() {
        #expect(jsonString("a\"b\\c\nd\te\u{1}—") == #""a\"b\\c\nd\te\u0001—""#)
    }

    // MARK: the transcript the real engine builds (no model call)

    @Test func theTranscriptReplaysInstructionsAndTurnsInOrder() throws {
        #if canImport(FoundationModels) && os(macOS)
            guard #available(macOS 26.0, *) else { return }
            let t = SystemFMEngine.transcript(
                FMRequest(
                    instructions: "be brief",
                    turns: [.init(role: "user", content: "hi"), .init(role: "assistant", content: "hello")],
                    prompt: "again"))
            let kinds = t.map { e -> String in
                switch e {
                case .instructions: return "instructions"
                case .prompt: return "prompt"
                case .response: return "response"
                default: return "other"
                }
            }
            #expect(kinds == ["instructions", "prompt", "response"])
            // no instructions entry at all when there are none
            let bare = SystemFMEngine.transcript(
                FMRequest(instructions: "", turns: [.init(role: "user", content: "x")], prompt: "y"))
            #expect(bare.count == 1)
        #endif
    }
}
