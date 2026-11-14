# E157 · chat_view_renders — the html chat view draws in a canvas cell

Scripted (`costs: free`): the model is the parity chat stub; its responder plays
three turns (a reply and a run of two calls, an inquiry, a closing reply).

**PASS** when every check holds:

| check | what |
|---|---|
| 1 | the cell draws the view (`#dialog`), the header names the chat and shows its model |
| 2 | the person's message is a right-hand bubble (`.who.user`) |
| 3 | the agent's reply is a bubble (`.who.assistant`) and its `` `ls` `` is a code chip |
| 4 | the two calls of one run are ONE row, `2 tool calls`, opening to two call entries |
| 5 | an inquiry to `user` is a card with its question and its options; the header says `asks you` |
| 6 | a click on an option answers it: the card is `answered`, the header's `asks you` is gone |
| 7 | after a page reload the cell draws the same history (bubbles, one tool row, the answered turn) |
| 8 | no page error |
