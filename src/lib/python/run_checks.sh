#!/bin/sh
# Staged verification: broadest test first, then everything.
#
# STAGE 1 is `tests/agentic` on purpose. Its general runner walks every config by
# glob and, per config, drives the deepest chain in the tree — manifest ->
# static_loader -> create_agent -> swallowed boot -> explicit boot -> persist ->
# read_tree -> Kernel.load -> per-agent boot -> roster compare. That crosses the
# loader, provider discovery, the record round-trip, validate_records, the
# two-sided gate and reflect in one pass, so a structural break surfaces HERE
# rather than as forty unrelated failures in stage 2.
#
# Fail fast at each stage: a broken stage 1 makes stage 2 unreadable.
set -e
cd "$(dirname "$0")"

echo "── 1/4  agentic (broadest: manifest -> boot -> persist -> reboot -> gate)"
uv run pytest tests/agentic -q --no-header -x

echo "── 2/4  full python suite"
uv run pytest -n auto -q --no-header

echo "── 3/4  ruff check"
uvx ruff check kernel/ main.py bundled_agents/ tests/

echo "── 4/4  ruff format"
uvx ruff format --check kernel/ main.py bundled_agents/ tests/

echo "ALL GREEN"
