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
**One leg is one principal**: every admitted request is handed to this leg
(`inbound_call`), which sends it AS ITSELF — its `can_call` and the target's
`callable_by` decide, and `X-Fantastic-Execution-Id` names are one caller's.
Everyone admitted to one leg is one tenant.
```
curl -X POST -H 'content-type: application/json' \
  -d '{"type":"reflect"}' http://<host>/<id>/<target_id>
```
