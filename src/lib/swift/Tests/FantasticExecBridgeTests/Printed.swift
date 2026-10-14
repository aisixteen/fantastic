// What a job printed, read the way a caller reads it now: `tail` over the job's
// line ring (`job_done` carries no output). One line per entry,
// each with its newline.

import FantasticJSON
import FantasticKernel

func printed(
    _ kernel: Kernel, _ agent: AgentId, _ done: JSON?, _ stream: String = "stdout"
) async -> String {
    guard let jid = done?["job_id"].asInt else { return "" }
    let r = await kernel.send([
        Item(
            agent,
            .object([
                "type": .string("tail"), "job_id": .integer(jid), "lines": .integer(500),
                "stream": .string(stream),
            ]))
    ])[0].result
    return (r["lines"].asArray ?? []).map { ($0["line"].asString ?? "") + "\n" }.joined()
}
