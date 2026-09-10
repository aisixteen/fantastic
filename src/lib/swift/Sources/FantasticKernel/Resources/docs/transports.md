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
                browser-pastable `GET .../<rest_id>/_reflect[/<target>][?readme=1]`.
- binary      — for byte-heavy payloads, a WS binary frame
                `[4-byte BE uint32 H | H-byte JSON header | M-byte body]`;
                `_binary_path` names the body field. Skips base64.

`kernel` is an alias for the tree root: send any verb to `kernel` and it
resolves to the root agent — handy for reflecting the whole tree without
knowing the root's id. To operate a running daemon's live surface (find its
port, POST/WS to it, stop it), `reflect kernel readme=daemon`.
