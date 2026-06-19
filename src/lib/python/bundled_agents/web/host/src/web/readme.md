# web — uvicorn HTTP host
Serves rendering only: `/` (tree), `/<id>/file/<path>` (static file proxy), favicon. No server-side `/<id>/` render route — frontend panels live in the TS kernel. Verb-invocation surfaces are sub-agents (web_ws, web_rest) that mount routes via the duck-typed `get_routes` verb. `port` field on the record is where it binds.
