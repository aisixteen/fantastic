cli — terminal render sink.

Receives `token`, `done`, `say`, `error`, `status`, `context` payloads and prints them to
stdout. Unknown verbs are silently DROPPED: this is a sink, and a render surface that
refused an unexpected event would turn a cosmetic gap into a failed send for its caller.

It NEVER inspects the tree. Producers push what they know — an agent announces its own
endpoints during boot — because the producer owns the information and this sink only
formats it.

Agents reach it by id (`cli`), so it is subject to the same two-sided gate as anything
else: an agent that streams here needs egress to `cli`, and `cli` needs `callable_by`
naming that agent.
