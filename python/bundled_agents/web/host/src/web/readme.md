# web — uvicorn HTTP host
Serves rendering only: `/` (tree), `/<id>/` (render_html), `/<id>/file/<path>`, transport.js, favicon. Verb-invocation surfaces are sub-agents (web_ws, web_rest) that mount routes via the duck-typed `get_routes` verb. `port` field on the record is where it binds.
