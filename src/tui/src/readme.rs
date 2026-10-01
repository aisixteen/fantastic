//! THE README a bare `fantastic` prints when it is NOT on a terminal (owner,
//! 2026-10-01: "in non-tui do not run w/o flags, just show readme"). A coding
//! CLI that runs `fantastic` in its shell reads what this is and how to drive
//! it; nothing is written to the directory and no kernel starts. On a terminal
//! the same command opens the terminal UI.

use fantastic_web_mcp::{INSTRUCTIONS, SHAPE};

const COMMANDS: &str = "Aisixteen Fantastic — a kernel of agents living in this directory (`.fantastic/`), driven by one verb, send, and discovered by reflect.\n\
\n\
This is `fantastic` without a terminal, so it only prints this readme: it starts nothing and writes nothing. Commands:\n\
\x20\x20fantastic                          the terminal UI (on a terminal); a fresh directory is seeded on launch\n\
\x20\x20fantastic canvas [ai] --port N     the terminal UI and the browser canvas (--port the first time only)\n\
\x20\x20fantastic --headless [canvas]      no screen: the kernel's message stream on stdout\n\
\x20\x20fantastic mcp                      MCP over stdio for a coding CLI (attaches to a running kernel, or runs it)\n\
\n";

/// The whole text: the commands, how to register the MCP face, and what a
/// connected MCP client is told.
pub fn text() -> String {
    format!("{COMMANDS}{SHAPE}{INSTRUCTIONS}\n")
}
