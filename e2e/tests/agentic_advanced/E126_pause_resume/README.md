# E126 pause_resume

NO MODEL, Python runtime. `pause`, three messages sent while paused, a kernel restart,
a fourth message, `resume`: the four held messages reach the model once each, in order,
after the resumed row, and none reached it while paused.

```sh
cd src/lib/python && uv run python ../../../e2e/tests/agentic_advanced/E126_pause_resume/probe.py
```
