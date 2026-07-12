> Every wire into this kernel — the envelope is always `{"type":"<verb>",...}`.

# Transports — every way to talk to this kernel

The envelope is always `{"type":"<verb>", ...fields}`. `reflect` is
universal; per-verb signatures come from each agent's reflect `verbs`.

- in_process  — `send(target_id, payload)` from code inside the kernel.
- in_prompt   — LLM tool-call loops emit
                `<send id="<agent_id>" payload='{"type":"<verb>", ...}'/>`.
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
- browser-bus — in a browser, `BroadcastChannel("fantastic")` carries
                `{type, target_id, source_id, ...}` between iframes,
                bypassing the kernel entirely (`fantastic_transport().bus`).

`kernel` is an alias for the tree root: send any verb to `kernel` and it
resolves to the root agent — handy for reflecting the whole tree without
knowing the root's id. To operate a running daemon's live surface,
`reflect kernel readme=daemon`.
