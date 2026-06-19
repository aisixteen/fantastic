// FantasticRunnerCore — shared `fantastic` lifecycle machinery behind a
// `RunnerTransport` seam. The local + ssh runner bundles supply only
// their transport conformance + a thin `AgentBundle` that dispatches
// verbs through `RunnerCore`.
//
// Mirrors the Rust `fantastic-runner-core` crate and the Python
// `runner_core` dedup (shared lifecycle + Transport seam), and follows
// the sibling `FantasticAICore` target's layout.
//
// Two runners, one lifecycle:
//
// - SHARED (this target): the verb dispatch skeleton — reflect / boot /
//   shutdown / start / stop, the transport-owned extra verb routing, and
//   the unknown-verb error string.
// - PER-TRANSPORT (in each runner target): how each verb does its work
//   and the concrete reply it returns. local = subprocess + tracked
//   children + OS signals; ssh = ssh exec + `ssh -L` tunnel.
//
// ## Runner contract (canonical reference)
//
// Every runner bundle implements the same lifecycle verbs so the canvas
// can drive a local or remote project identically.
//
// ### Verbs (caller → runner, via `kernel.send`)
//
// - `reflect` — identity + verb catalogue + live status. No args.
// - `boot` — no-op (`{ok:true}`); runners do NOT auto-start.
// - `start` — bring the project up (idempotent).
// - `stop` — tear the project down (idempotent).
// - `shutdown` — drain ALL live work, then `{ok:true}`.
// - transport-specific: local `list` (active children), ssh `status`
//   (tunnel + remote liveness).
//
// Reply *shapes* differ per transport (local carries `pid` / a
// `children` list, ssh carries `local_port` / `remote` / tunnel `pid`)
// and are owned by the transport — see each runner target.
//
// This target is pure cross-platform — NO `#if os(macOS)`. The macOS
// gating stays on the runner bundles that conform to `RunnerTransport`.
