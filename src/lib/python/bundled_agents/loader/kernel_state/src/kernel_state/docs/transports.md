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
                `{"type":"call","target":..,"payload":..,"id":..}`.
- rest        — `POST http://host:port/<rest_id>/<target_id>` body=payload;
                header `X-Fantastic-Execution-Id` (the item's `id`);
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

**Who is calling.** Identity comes only from dispatch: a send made inside an
agent's handler (or a task it spawned) is that agent's; a send made outside every
dispatch — the cli, a test driver — is the operator's. A wire caller is not an
identity here: the leg that admits its frame (ws, rest, a bridge leg) performs the
call AS ITSELF, so the leg's `can_call` and the target's `callable_by` decide.

**Agents on another kernel** are addressed `<guid>|<agent id>` and called with an
ordinary `send` — no special verb. The root reflect lists the kernels this one is
connected to as `peers: [{guid, via}]`; send `reflect` to `<guid>|kernel` for that
kernel's tree and verbs. Your `can_call` must name the target (`<guid>|<agent>`, or
`<guid>|*` for every agent there); that kernel's own gate decides whether the target
accepts the call, judged as the leg that serves it there. A guid not in `peers` is
an unknown agent. A
browser page becomes a peer when its kernel sends `{"type":"hello","guid":..}` on
its ws; the host's calls then arrive as `{"type":"call","id":..,"target":..,
"payload":..}` frames the page answers with
`{"type":"reply"|"error","id":..,"data":..}`. The tabs of one identity say hello
with ONE guid and are one principal: a call to `<guid>|<agent>` goes to every live
tab and settles with the first reply; `peers` lists one row per live connection.

To operate a running daemon's live surface (find its
port, POST/WS to it, stop it), `reflect kernel readme=daemon`.
