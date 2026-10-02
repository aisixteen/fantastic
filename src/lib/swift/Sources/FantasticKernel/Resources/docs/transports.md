> Every wire into this kernel — a payload always names its verb in `type`; a MODEL writes it as `<call>` tags, a program as JSON.

# Transports — every way to talk to this kernel

A payload always names its verb in `type` (JSON on the program wires below; tags in a
model's prompt). `reflect` is universal; per-verb signatures come from each agent's
reflect `verbs`.

- in_process  — `send(target_id, payload)` from code inside the kernel.
- in_prompt   — an AI agent emits a `<call>` element, one child tag per
                argument, NO JSON: `<call name="send"><target_id>x</target_id>
                <type>verb</type><path>a.md</path><bytes>…</bytes></call>`.
- cli         — `fantastic <agent_id> <verb> [k=v ...]`; shorthand
                `fantastic reflect [<agent_id>]`. One-shot; refused while a
                daemon owns the dir (use the web surface then).
- ws          — `ws://host:port/<agent_id>/ws`; frames
                `{"type":"call","target":..,"payload":..,"id":..,"secret":[..]?}`.
- rest        — `POST http://host:port/<rest_id>/<target_id>` body=payload;
                headers `X-Fantastic-Execution-Id` (the item's `id`) and
                `X-Fantastic-Secret` (the item's `secret`, a JSON array);
                browser-pastable `GET .../<rest_id>/_reflect[/<target>][?readme=1]`.
- binary      — for byte-heavy payloads, a WS binary frame
                `[4-byte BE uint32 H | H-byte JSON header | M-byte body]`;
                the header holds `null` where the body belongs and
                `_binary_path`, the steps to it from the frame root: a
                key as a string, taken literally, a list index as an
                int — `["payload","bytes"]`, `["payload","a.b"]` (the
                key `a.b`), `["data","items",0,"data"]`. Skips base64.

`kernel` is a reserved address for the tree root: send any verb to `kernel`
and it resolves to the root agent, whatever its id — reflect the whole tree
without knowing the root's id.

**Agents on another kernel** are addressed `<guid>|<agent id>` and called with an
ordinary `send` — no special verb. The root reflect lists the kernels this one is
connected to as `peers: [{guid, via}]`; send `reflect` to `<guid>|kernel` for that
kernel's tree and verbs. Your `can_call` must name the target (`<guid>|<agent>`, or
`<guid>|*` for every agent there); that kernel's own gate decides whether the target
accepts you, as `<this guid>|<you>`. A guid not in `peers` is an unknown agent. A
browser page becomes a peer when its kernel sends `{"type":"hello","guid":..}` on
its ws; the host's calls then arrive as `{"type":"call","id":..,"target":..,
"payload":..,"caller":..,"secret":[..]?}` frames the page answers with
`{"type":"reply"|"error","id":..,"data":..}`.

**Secret marks cross every wire.** An item's `secret` paths (see `system-verbs`)
ride its call wherever it goes — the `secret` field of a ws / bridge `call` frame,
the `X-Fantastic-Secret` header on rest, the frame a `<guid>|<agent>` send puts on
the peer's wire, a spawned program's `{op:"send", …, secret}` on the exec_bridge
guest wire — and the RECEIVING kernel dispatches the item with them, so it
records `<secret>` exactly as the calling kernel does; a malformed mark is that
kernel's own refusal, before dispatch. On a bridge's `forward`, mark YOUR call:
`payload.payload.<field>` rides as `payload.<field>`, `result[.<field>]` as is.
A RELAY leg (`relay_connector`) is the exception — SEALED: the relay records the
frames it routes unmasked, so a call carrying any mark is refused there, both ways;
send secrets over a direct `ws_bridge` leg. To operate a running daemon's live surface (find its
port, POST/WS to it, stop it), `reflect kernel readme=daemon`.
