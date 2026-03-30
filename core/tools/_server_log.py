"""Server log buffer — captures main process log messages into a ring buffer."""

import collections
import logging

from ..dispatch import ToolResult, register_dispatch, register_tool

# ─── Constants ───────────────────────────────────────────────────────────

SERVER_LOG_BUFFER_SIZE = 100  # max log entries kept in memory


# ─── Ring buffer + handler ───────────────────────────────────────────────

_log_buffer: collections.deque[dict] = collections.deque(maxlen=SERVER_LOG_BUFFER_SIZE)


class _BufferingHandler(logging.Handler):
    """Logging handler that appends formatted records to the ring buffer."""

    def emit(self, record: logging.LogRecord) -> None:
        try:
            _log_buffer.append(
                {
                    "ts": record.created,
                    "level": record.levelname,
                    "name": record.name,
                    "message": self.format(record),
                }
            )
        except Exception:
            pass  # never break the logging chain


_handler_installed = False


def install_log_buffer() -> None:
    """Attach the buffering handler to the root logger. Idempotent."""
    global _handler_installed
    if _handler_installed:
        return
    handler = _BufferingHandler()
    handler.setLevel(logging.INFO)
    handler.setFormatter(
        logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s")
    )
    root = logging.getLogger()
    root.addHandler(handler)
    # Ensure root logger passes INFO+ to our handler (uvicorn may reset it)
    if root.level > logging.INFO or root.level == logging.NOTSET:
        root.setLevel(logging.INFO)
    _handler_installed = True


# ─── Tool ────────────────────────────────────────────────────────────


@register_dispatch("server_logs")
async def _server_logs(max_lines: int = SERVER_LOG_BUFFER_SIZE) -> ToolResult:
    entries = list(_log_buffer)
    if len(entries) > max_lines:
        entries = entries[-max_lines:]
    return ToolResult(
        data={
            "lines": len(entries),
            "entries": entries,
        }
    )


@register_tool("server_logs")
async def server_logs(max_lines: int = SERVER_LOG_BUFFER_SIZE) -> list[dict]:
    """Read the main process server log buffer.

    Returns the last N log entries from this Fantastic instance's server process.
    Each entry has: ts (unix timestamp), level, name (logger), message.

    Args:
        max_lines: Maximum number of log entries to return (default 100).
    """
    tr = await _server_logs(max_lines)
    return tr.data["entries"]
