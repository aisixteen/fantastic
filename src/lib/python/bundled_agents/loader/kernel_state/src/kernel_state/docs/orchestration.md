> Any routine reaches every agent by id — memory, inference, compute are interchangeable.

# Meta-possibility — any routine orchestrates the whole substrate

Every routine reaches every agent by id through its connector — a host
`python_runtime` job (its spawned code gets a `kernel`:
send/emit/reflect/watch/on_message) and a browser view-agent's JS (its
injected `fantastic`) alike. So from EITHER kernel a routine can: read
memory from anywhere (`send(<state_id>, {type:"read"})`), run an inference
turn (`send(<ai_id>, {type:"send", system_prompt, text})`), and/or spawn a
compute job (`send(<py_id>, {type:"start", code})`) — by id, regardless of
which kernel owns the target.

Memory, inference, and compute are interchangeable units you wire from
anywhere: a python routine can call an AI; an AI or a JS panel can spawn a
python routine; all read the same memory. This is why a step written as code
and a step written as an LLM call are substitutable. Bind by id + duck-typed
verbs, never by concrete type.

An AI worker's result is not plumbed: the per-call prompt names who listens,
the system prompt carries the `send` signature, and the model routes its own
output (to one addressee or many).
