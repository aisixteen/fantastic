# E65 load

A canvas under load, measured: `LOAD_N` two-way terminals (each streams a line
every 200 ms; a sample is typed into) over a heavy shader background. It reports
keystroke→echo latency, frame rate, row-store rows/s by kind and by agent/verb,
store growth on disk, and host call lines/s. A disk guard (`LOAD_DISK_MB`,
default 200) stops the load before the store fills the disk.

Step it: `LOAD_N=10`, then `100`, then `1000` — never straight to 1000.

    cd src/lib/python && LOAD_N=10 uv run python ../../tui/e2e/tests/canvas/E65_load/probe.py
