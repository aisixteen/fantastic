// A FENCED `send_binary` must reach the target's BINARY channel.
//
// `BinaryBatchTests` proves the kernel half — an `Item` carrying a blob is
// dispatched through `handleBinary` under its own task row. Nothing proved the
// AI-CORE half: that routing lifts the fenced program off the JSON header and
// onto the item. It did not, and e2e `E10_dialog:swift` caught it — the model
// wrote a correct fenced call, the blob stayed a string in the header, the item
// went out on the TEXT channel and `file_bridge` answered
// `{"error":"write_stream: bytes must be raw bytes on the binary channel"}`
// (measured 2026-09-03). The model then repeated the same correct call and
// failed the run.

import FantasticJSON
import FantasticKernel
import Foundation
import Testing

@testable import FantasticAICore

@Suite("send_binary routes to the binary channel")
struct SendBinaryRouteTests {

    /// The fenced form exactly as a model writes it: the JSON line with the
    /// `"@body"` placeholder, then the program between column-0 markers.
    private func fenced(target: String, program: String) -> String {
        "<tool_call>\n"
            + #"{"name":"send_binary","arguments":{"target_id":"\#(target)","payload":"#
            + #"{"type":"write_stream","path":"answer","bytes":"@body","truncate":true}}}"# + "\n"
            + "*** Begin Bytes\n" + program + "\n*** End Bytes\n</tool_call>"
    }

    @Test func aFencedSendBinaryLandsOnTheBinaryChannel() async {
        let script = ProviderScript([["ack " + doneEnvelope]])
        let (kernel, ids, recorder) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        await routeReply(
            selfId: ids["mock"]!, kernel: kernel,
            text: fenced(target: "probe", program: "42"),
            clientId: "c", sendTo: nil, systemPrompt: nil, incoming: "",
            sendTimeout: 5.0)

        let bins = recorder.binaries()
        let onText = recorder.payloads().map { $0.serialize() }.joined(separator: " | ")
        #expect(
            bins.count == 1,
            "the blob never reached the binary channel; text channel saw \(onText)")
        guard let (header, blob) = bins.first else { return }
        // The fence body is everything BETWEEN the markers — the program's own
        // trailing newline included, as python keeps it.
        #expect(String(data: blob, encoding: .utf8) == "42\n", "bytes must ride verbatim")
        #expect(header["type"].asString == "write_stream")
        #expect(header["path"].asString == "answer")
        #expect(header["truncate"].asBool == true, "the rest of the header still rides")
        #expect(
            header["bytes"] == .null,
            "the program must be LIFTED off the header, not duplicated into it")
        #expect(recorder.payloads().isEmpty, "nothing may reach the text channel")
    }

    @Test func theReplyTagNamesARealPid() async {
        // It rides the ordinary batch, so it has a task row and a pid — the
        // reply the model reads must name it, not `-`.
        let script = ProviderScript([["ack " + doneEnvelope], ["done " + doneEnvelope]])
        let (kernel, ids, _) = await bootMockKernel(
            config: scriptedConfig(["mock": script]), backendIds: ["mock"])
        await routeReply(
            selfId: ids["mock"]!, kernel: kernel,
            text: fenced(target: "probe", program: "x"),
            clientId: "c", sendTo: nil, systemPrompt: nil, incoming: "",
            sendTimeout: 5.0)
        #expect(await script.waitForCalls(1), "the reply block never came back")
        let tag = script.requestText(0)
        #expect(tag.contains("[reply to probe:"), "no real pid in the tag: \(tag.suffix(300))")
    }
}
