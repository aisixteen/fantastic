#!/bin/sh
# Kill STRAY kernels only: a kernel whose parent is gone (reparented to launchd,
# ppid 1) or whose parent is not a live process. A kernel under a live e2e lane /
# capture / test runner keeps its parent and is never touched.
#   e2e/kill_strays.sh        # kill
#   e2e/kill_strays.sh -n     # dry run: list what would be killed
dry=0; [ "$1" = "-n" ] && dry=1
ps -Ao pid=,ppid=,command= | grep -E 'e2e/dist/(python|rust|swift)/|src/lib/python/main\.py|fantastic_kernel|target/(debug|release)/fantastic mcp' \
 | grep -vE 'grep|kill_strays' | while read -r pid ppid cmd; do
    if [ "$ppid" = "1" ] || ! kill -0 "$ppid" 2>/dev/null; then
        echo "stray $pid (ppid $ppid): $(printf '%s' "$cmd" | cut -c1-90)"
        [ $dry = 1 ] || kill -9 "$pid" 2>/dev/null
    fi
done
