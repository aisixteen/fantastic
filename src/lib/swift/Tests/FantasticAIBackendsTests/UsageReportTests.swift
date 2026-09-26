// PROVIDER-REPORTED USAGE and the STOP REASON — what each wire says about a pass
// beside its text. ai_core calibrates its chars/4 estimate with the reported
// prompt size, and a fold refuses a summary the provider cut at `max_tokens`.
//
// Twins of the python providers' `last_prompt_tokens` / `last_stop_reason`
// (anthropic `message_start` usage + `message_delta` stop_reason; the OpenAI
// wire's `usage.prompt_tokens` + `finish_reason`; ollama's `prompt_eval_count` +
// `done_reason`), `length` read as `max_tokens` everywhere.

import FantasticAICore
import FantasticJSON
import Foundation
import Testing

@testable import FantasticAnthropicBackend
@testable import FantasticOllamaBackend

@Suite("usage + stop reason")
struct UsageReportTests {
    @Test func theOpenAIWireReportsPromptTokensAndALengthStop() {
        let usage = openAIReport(
            #"data: {"choices":[],"usage":{"prompt_tokens":1234,"completion_tokens":5}}"#)
        #expect(usage.promptTokens == 1234 && usage.stopReason == nil)
        let cut = openAIReport(#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#)
        #expect(cut.stopReason == "max_tokens" && cut.promptTokens == nil)
        let stop = openAIReport(#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#)
        #expect(stop.stopReason == "stop")
        #expect(openAIReport("data: [DONE]").promptTokens == nil)
        #expect(openAIReport(#"data: {"choices":[{"delta":{"content":"x"}}]}"#).stopReason == nil)
        let u = AIUsage()
        recordOpenAIReport(#"data: {"choices":[],"usage":{"prompt_tokens":77}}"#, into: u)
        recordOpenAIReport(#"data: {"choices":[{"finish_reason":"length"}]}"#, into: u)
        #expect(u.lastPromptTokens == 77 && u.lastStopReason == "max_tokens")
    }

    @Test func ollamaReportsPromptEvalCountAndADoneReason() {
        let r = ollamaReport(#"{"done":true,"done_reason":"length","prompt_eval_count":321}"#)
        #expect(r.promptTokens == 321 && r.stopReason == "max_tokens")
        #expect(ollamaReport(#"{"done":true,"done_reason":"stop"}"#).stopReason == "stop")
        #expect(ollamaReport(#"{"message":{"content":"x"}}"#).promptTokens == nil)
    }

    @Test func anthropicMessageStartReportsTheWholeWindowCacheIncluded() {
        var st = AnthropicStreamState()
        let d = decodeAnthropicSSELine(
            #"data: {"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5}}}"#,
            state: &st)
        #expect(d.promptTokens == 105)
        // `/cc`-style routes stream 0 here: reported as nothing, never as 0
        let zero = decodeAnthropicSSELine(
            #"data: {"type":"message_start","message":{"usage":{"input_tokens":0}}}"#, state: &st)
        #expect(zero.promptTokens == nil)
    }

    @Test func aBoundedAnthropicSideCallCarriesItsOwnMaxTokens() {
        let p = AnthropicProvider(
            endpoint: "http://x.invalid", model: "m", apiKey: "k", maxTokens: 4096, sampling: [:])
        let msgs: [JSON] = [.object(["role": .string("user"), "content": .string("hi")])]
        #expect(p.requestBody(messages: msgs)["max_tokens"] == .integer(4096))
        #expect(p.requestBody(messages: msgs, maxTokens: 1500)["max_tokens"] == .integer(1500))
    }
}
