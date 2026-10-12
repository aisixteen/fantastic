"""A starter python agent: its VERBS are the functions passed to fa.serve.

Editing this file restarts the agent at once (the dev watcher reloads the bundle).
Add a verb: write the function, add it to fa.serve AND to bundle.yaml `verbs` (the
two must match — the kernel refuses an agent whose hello differs).
"""
import fantastic_agent as fa

count = 0


def hello(p):
    return {"greeting": f"hello, {p.get('name') or 'world'}"}


def bump(p):
    global count
    count += 1
    fa.emit({"type": "count", "count": count})  # its panel watches this
    return {"count": count}


fa.serve({"hello": hello, "count": bump})
