# Scenario: /intro movie

Trigger the scripted intro "movie" from the chat and stream frames to see the
scenes animate. (The same movie auto-plays after 10s idle on the attract screen.)

```script
wait 1500
key space
wait 500
type /intro
key enter
# stream ~6s of the movie as frames every 500ms
stream 500 12
key space
wait 300
shot back_to_chat
```

## Expect
- The `stream` frames show the movie's scenes (NOT the attract title — that scene
  was moved to the attract screen): e.g. a `send(target, payload)` packet moving
  between `[core]` and `[web]` boxes, a `reflect` agent self-describing, a
  composed agent graph, the brain firing packets, and a marquee credits scroller.
  Different stream frames show different scenes / motion.
- `back_to_chat`: pressing a key stops the movie and returns to the chat (the
  input prompt is visible again).

## Pass / fail
PASS if the stream shows animated movie scenes and any key returns to chat. FAIL
if `/intro` does nothing, or a key doesn't dismiss it.
