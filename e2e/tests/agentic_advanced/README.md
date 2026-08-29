# `agentic_advanced/` — structures that outlive the conversation

The hard end. Not "can a model call a verb" but "can it leave behind something
that keeps working after the builder walks away".

| test | what it proves |
|---|---|
| `subai` | it builds durable sub-agents that still work once the conversation ends |
| `subai_graph` | the same build, scored as a GRAPH — edges, not just nodes |
| `graph_shape` | A/B: does naming the graph change the shape the model builds? |

**Wiring here includes granting authority.** The substrate is deny-all: creating
an agent connects it to nothing, and a builder that only creates agents has built
a pile, not a graph. `subai`'s task says so explicitly — it did not always, and
the omission cost real runs that scored as model failures while the fixture was
withholding grants the task required. `selftest.py` exists because of that.

These are the most expensive tests in the suite: long idle windows, high turn
caps, and several model-driven agents alive at once.
