# web_rest — inbound HTTP leg

`POST /<id>/<target>` body=`{"type":"<verb>",...}` → JSON reply.
`GET /<id>/_reflect[/<target>][?readme=1]` — reflect shortcuts.
**Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`.
Token on **`X-Fantastic-Auth` header** (not the envelope — HTTP has no envelope).
Name an execution with **`X-Fantastic-Execution-Id`** — same id twice returns the
first outcome instead of running the verb again (dispatch metadata on the
carrier, never in the body).
Mark secret values with **`X-Fantastic-Secret`** — a JSON array of paths, the
item's `secret` (`["payload.fields"]`, `["result"]`): the reply is real, every
record of the call shows `<secret>`. Not JSON → `400`; a bad mark → the kernel's
`bad_arg` refusal.
```
curl -X POST -H 'content-type: application/json' \
  -H 'X-Fantastic-Secret: ["payload.fields"]' \
  -d '{"type":"set","ref":"openai/main","host":"api.openai.com","fields":{"api_key":"…"}}' \
  http://<host>/<id>/<creds_id>
```
