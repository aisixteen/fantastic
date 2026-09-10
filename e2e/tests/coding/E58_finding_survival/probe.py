"""coding/finding_survival — the model must NOTICE something, not copy a rule.

`fact_survival` proved the pipe: a rule stated as "line N must be exactly X" gets
checkpointed verbatim and survives. That is the friendly case. This is the case that
tests JUDGMENT: one of eight identical-looking fillers carries an audit marker buried in
the MIDDLE of its body — not the head, not the tail, nowhere a preview would show it. The
model reads it once, early; ~7 cuts later the read is long gone; the report must name the
file and the token.

Nothing tells the model to checkpoint THIS. The howto says "dump findings"; whether the
model recognises a finding when it sees one is exactly what is being measured. The token
appears in one filler body and nowhere else.
"""

from __future__ import annotations

import asyncio
import secrets
import sys
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import verdict
from helpers.facts_arm import (
    FOLD_ARMS,
    LOREM,
    base_checks,
    both_arms,
    conclude,
    fixture,
    report,
)
from helpers.kernel import open_kernel

BK = _resolve_backend()
FB = "find_fb"
N = 8
DONE = 'end your run by calling done — emit <call name="done"></call>'
TASK = (
    f"Survey the corpus: read `filler_0.txt` through `filler_{N - 1}.txt` ({N} files) "
    "through the editor `notes`, ONE PER TURN. Some files may carry an audit marker "
    "line — note anything unusual as you go. Then write `report.md` through `notes`: "
    "list every file you read, one per line, and if any file carried an audit marker, "
    "add the line `marker: <filename> <marker value>`. Do not re-read files. "
    f"Then {DONE}."
)


async def main():
    root, core, close = await open_kernel(
        prefix="coding_find_", reach=[FB, "notes", "ai_recency", "ai_fold", "cli", "user"]
    )
    try:
        token = secrets.token_hex(4)
        hit = 2  # early enough that every cut has taken it by the end
        half = len(LOREM) // 2
        for i in range(N):
            body = f"chunk {i}: {LOREM}"
            if i == hit:
                body = (f"chunk {i}: {LOREM[:half]}\n>> audit marker: {token}\n"
                        f"{LOREM[half:]}")
            Path(f"filler_{i}.txt").write_text(body + "\n")
        assert token not in TASK
        await fixture(root, FB, arms=FOLD_ARMS)
        recency, facts = await both_arms(root, core, BK, FB, TASK, "report.md", arms=FOLD_ARMS)

        want = f"marker: filler_{hit}.txt {token}"

        def found(a):
            b = a["body"]
            return want in b, (token in b)

        report((recency, facts), lambda a: (
            f"FOUND exact={found(a)[0]}  token anywhere={found(a)[1]}  "
            f"checkpointed the token={any(token in f['text'] for f in a['stored'])}"))
        def distilled(a):  # the model SAID what the read told it, in its own turn
            return any(m.get("role") == "assistant" and token in str(m.get("content"))
                       for m in a["msgs"])

            # NOT "both arms actually compacted": folding here was a side effect of a
            # BLIND grep (2026-09-08: 12 greps, 12 zeros, then pulls filled the window).
            # With grep reaching stored bodies (fixed 2026-09-09) the model never needs
            # to hold them, so this test measures FACT SURVIVAL; folding is measured by
            # fold_paging / nested_folds / superlong_chain / user_fold.
        checks = [c for c in base_checks(recency, facts) if not c[0].startswith(('both arms produced', 'both arms actually compacted'))] + [
            ("the fold arm produced the artifact", bool(facts['body'].strip())),
            ("the fold arm wrote the marker into its own prose when it saw it",
             distilled(facts)),
            ("the fold arm reported the file and token exactly", found(facts)[0]),
        ]
        print(f"\n  MEASURED — marker reported:  recency={found(recency)[0]}  "
              f"facts={found(facts)[0]}")
        allok, note = conclude(recency, facts, checks, "noticed, checkpointed unprompted, reported after the read was gone", "a finding_survival check failed")
        verdict(allok, note)
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
