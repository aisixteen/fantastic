# file — filesystem as an agent
Verbs: read, write, list, delete, rename, mkdir. Rooted at the `root` field; path-safety refuses anything escaping it. Serve files over HTTP via `/<file_id>/file/<path>`.
