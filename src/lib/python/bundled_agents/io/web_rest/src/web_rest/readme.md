# web_rest — inbound HTTP leg

`POST /<id>/<target>` body=`{"type":"<verb>",...}` → JSON reply.
`GET /<id>/_reflect[/<target>][?readme=1]` — reflect shortcuts.
**Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`.
Token on **`X-Fantastic-Auth` header** (not the envelope — HTTP has no envelope).
**Gate first, then the body**: a POST is judged on its target and `X-Fantastic-Auth`
before one body byte is read — a refused client's body is never read. An admitted
body is read up to **16 MiB** (`MAX_BODY` = io_bridge `MAX_FRAME`, the one frame cap
every wire leg holds); larger → `413` `{reason:"transport_error", limit}` ("the
request body exceeds 16777216 bytes …") — a declared `Content-Length` over it is
refused unread. Send more in chunks (`write_stream`).
Name an execution with **`X-Fantastic-Execution-Id`** — same id twice returns the
first outcome instead of running the verb again (dispatch metadata on the
carrier, never in the body).
Mark secret values with **`X-Fantastic-Secret`** — a JSON array of marks, the
item's `secret`: dotted strings (`["payload.fields"]`, `["result"]`) and/or arrays
of steps for a key holding a dot (`[["payload","a.b"]]`): the reply is real, every
record of the call shows `<secret>`. Not JSON → `400`; a bad mark → the kernel's
`bad_arg` refusal.
**One leg is one principal** (audit S6, 2026-10-03): every request admitted to this
leg is dispatched AS the leg, so grants, a credential_store's `readers`,
`X-Fantastic-Execution-Id` names and a secret-marked execution's join (the real
outcome goes to its original caller — the leg) see ONE caller. Everyone admitted to
one leg is one principal — one tenant per leg.
```
curl -X POST -H 'content-type: application/json' \
  -H 'X-Fantastic-Secret: ["payload.fields"]' \
  -d '{"type":"set","ref":"openai/main","fields":{"api_key":"…"}}' \
  http://<host>/<id>/<creds_id>
```
