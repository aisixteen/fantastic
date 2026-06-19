"""Stdlib-only `.env` autoloader. Existing os.environ entries win."""

from __future__ import annotations

import os
from pathlib import Path


def _load_dotenv(path: Path = Path(".env")) -> int:
    """Load `KEY=value` pairs from `.env` in cwd into os.environ.

    Stdlib only. Silent no-op when the file is absent. Existing
    os.environ entries are NEVER overwritten — the shell wins, so a
    `NVAPI_KEY=...` exported in the parent shell beats whatever .env
    says. Returns the count of vars actually set.

    Accepted format:
      - `KEY=value`, one per line
      - blank lines and `#` comment lines skipped
      - leading `export ` tolerated (so the same file works in bash)
      - matching surrounding quotes stripped from value (`"..."` / `'...'`)
      - no variable expansion, no multiline values, no escapes
    """
    if not path.exists():
        return 0
    count = 0
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line[len("export ") :].lstrip()
        if "=" not in line:
            continue
        key, _, val = line.partition("=")
        key = key.strip()
        val = val.strip()
        if not key:
            continue
        if len(val) >= 2 and val[0] == val[-1] and val[0] in ("'", '"'):
            val = val[1:-1]
        if key not in os.environ:
            os.environ[key] = val
            count += 1
    return count
