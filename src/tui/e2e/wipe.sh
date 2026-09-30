#!/bin/sh
# Wipe every canvas e2e run's artifacts (src/tui/e2e/runs/): host logs, project
# dirs, page DOMs, summaries. Safe: nothing else lives there.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
rm -rf "$here/runs"
echo "wiped $here/runs"
