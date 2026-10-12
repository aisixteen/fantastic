> One routing rule: frontend code talks only to the browser JS kernel.

# How it routes (one rule: only the JS kernel)

The connector talks ONLY to the browser JS kernel — never the host directly.
The JS kernel is the SOLE owner of the host link and abstracts local-vs-host,
so you address EVERY agent by id the same way:

- another FRONTEND agent (e.g. an html agent) → delivered locally, in-browser.
- a HOST agent (e.g. an `exec_bridge` you `create_agent` here) → routed
  over the kernel bridge; its reply / emitted events come back the same way.
- a HOST agent PUSHES to a frontend agent the same way: it emits on its own id;
  the frontend agent `watch`es it and receives. Host backends a view fronts are weak PEERS
  by id — closing a view leaves the backend running.

Frontend code NEVER addresses the host directly — always the JS kernel.
