> Think with an AI agent — it does ONE pass and has no loop, so the control flow is YOURS to author.

# Minds — thinking, and the loop you build around it

Some agents in this kernel are **AI agents** (a "brain"): an agent whose body is a
language model. Reflect the tree and you will find them; their `verbs` carry `send`.

## What an AI agent IS — and what it is NOT

- **Call it** — `send('<ai_agent_id>', {type:'send', text:'<the turn>', system_prompt:'<YOUR instructions to it>', client_id:'<who asked>'})`. It replies `{response:'<the completion text>'}`. That is all it does.
- **It does ONE pass.** It reads, it completes, it stops.
- **It has NO loop, NO tools, NO agency.** It cannot call another agent. It cannot decide to try again. It will not act on the world. It only ever hands you TEXT.
- **It DOES own the conversation.** It keeps the history per `client_id` and compacts it when the window overflows — you never manage context.

## The consequence — this is the whole idea

Because the AI agent stops after one pass, **every bit of control flow is yours to
author**: looping, planning, retrying, using other agents as tools, judging when the
work is actually done, and handing a decision back to the human. There is no built-in
agent loop anywhere in this kernel. If you want an agent that plans and executes, **you
write that loop** — as a script agent (see the `self-extension` doc), with an AI agent call
where the thinking goes.

So an LLM call is just a **unit you can place anywhere**: wherever you would otherwise
hand-write a decision, a classifier, or a transform, you can `send` to an AI agent instead —
and the reverse. Compute, inference, and memory are interchangeable things addressed by
id.

## Thinking with it

The AI agent knows no conventions but the ones you give it. **You teach it how to answer**
in `system_prompt` — for example "reply with exactly one line: `PLAN: …` or `ASK: …` or
`FINAL: …`" — then you read `reply.response` as plain TEXT, parse it yourself, and act.
Keep the convention simple enough that a split cannot get it wrong; a weak model will
break a nested format long before it breaks a line.

A **mind** is nothing more than: an agent whose loop drives an AI agent.

```
receive a turn (on_message)
  → send it to the AI agent (with your system_prompt)
  → read the completion, decide what it means
  → act: send to other agents (they are your tools), feed the results back as the next turn
  → repeat until the work is genuinely done
  → emit say, then done
```

## Handing a decision back to the human

When the choice is the human's to make, do not guess — **ask**, and suspend:

- emit `{type:'question', request_id:'<unique>', kind:'single', question:'<text>', options:[{value:'<a>'},{value:'<b>'}], source:kernel.self_id, client_id:'<caller>'}` — `kind` is `text` | `single` | `multi` | `confirm`;
- then `on_message()` again: the answer arrives as `{type:'answer', request_id:'<same>', action:'accept', value:<what they chose>}` (`action` may instead be `decline`/`cancel`, which carry NO value — branch, never invent one);
- use the answer as the next turn, and finish with `say` + `done` as usual.

## Making your loop visible

A loop nobody can see is a loop nobody trusts. Narrate it and a UI will render it —
nothing here is hard-coded, the surface simply draws what agents emit:

- `{type:'plan', steps:['<a>','<b>'], client_id}` — announce the plan you just made;
- `{type:'step', index:<n>, total:<n>, text:'<what you are doing>', status:'running'|'done', client_id}` — advance it.

Emit them or don't; the loop is yours either way.
