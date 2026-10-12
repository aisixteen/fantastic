// PROVIDER-REPORTED USAGE and the STOP REASON — what each wire says about a pass
// beside its text. ai_core calibrates its chars/4 estimate with the reported
// prompt size, and a fold refuses a summary the provider cut at `max_tokens`.
//
// Twins of the python providers' `last_prompt_tokens` / `last_stop_reason`
// (ollama's `prompt_eval_count` + `done_reason`), `length` read as `max_tokens`.

import FantasticAICore
import FantasticJSON
import Foundation
import Testing

@testable import FantasticOllamaBackend

@Suite("usage + stop reason")
struct UsageReportTests {
    @Test func ollamaReportsPromptEvalCountAndADoneReason() {
        let r = ollamaReport(#"{"done":true,"done_reason":"length","prompt_eval_count":321}"#)
        #expect(r.promptTokens == 321 && r.stopReason == "max_tokens")
        #expect(ollamaReport(#"{"done":true,"done_reason":"stop"}"#).stopReason == "stop")
        #expect(ollamaReport(#"{"message":{"content":"x"}}"#).promptTokens == nil)
    }
}
