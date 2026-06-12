# web_rest — inbound HTTP leg

`POST /<id>/<target>` body=`{"type":"<verb>",...}` → JSON reply.
`GET /<id>/_reflect[/<target>][?readme=1]` — reflect shortcuts.
**Sealed by default** — open: `update_agent <id> ingress_rule=allow_all`.
Token on **`X-Fantastic-Auth` header** (not the envelope — HTTP has no envelope).
