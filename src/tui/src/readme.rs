//! THE README a bare `fantastic` prints when it is NOT on a terminal (owner,
//! 2026-10-01: "in non-tui do not run w/o flags, just show readme"). A coding
//! CLI that runs `fantastic` in its shell reads what this is and how to drive
//! it; nothing is written to the directory and no kernel starts. On a terminal
//! the same command opens the terminal UI.

use fantastic_web_mcp::{INSTRUCTIONS, LEAD_IN, SHAPE};

const COMMANDS: &str = "Aisixteen Fantastic — a kernel of agents living in this directory (`.fantastic/`), driven by one verb, send, and discovered by reflect.\n\
\n\
This is `fantastic` without a terminal, so it only prints this readme: it starts nothing and writes nothing. Commands:\n\
\x20\x20fantastic                          the terminal UI (on a terminal); a fresh directory is seeded on launch\n\
\x20\x20fantastic canvas [ai] --port N     the browser canvas, headless: the kernel's message stream on stdout (--port the first time only)\n\
\x20\x20fantastic [canvas] ai vllm -p PORT  the root AI `ai` on a backend (also `ai ollama -p PORT`, `ai responses|anthropic --model M`)\n\
\x20\x20fantastic [canvas] run CMD [ARGS]   your own program from $PATH, as is, in the terminal (or a canvas cell)\n\
\x20\x20fantastic --headless               no screen, no canvas: the kernel's message stream on stdout\n\
\x20\x20fantastic mcp                      MCP over stdio for a coding CLI (attaches to a running kernel, or runs it)\n\
\x20\x20fantastic send '<item json>'       one call into the running kernel, as the `mcp` leg; `$env:NAME` values come from this shell, marked secret\n\
\x20\x20fantastic readme [key]              the docs: the index, or one doc (`canvas`, `mcp`, `setup`, `bundles`, and the kernel's own)\n\
\n";

/// The CLI's own docs, beside the kernel's embedded tree
/// (`fantastic_core::DOCS`). Each opens with a `> ` one-line summary, as the
/// kernel's do.
const CLI_DOCS: &[(&str, &str)] = &[
    ("canvas", include_str!("../docs/canvas.md")),
    ("mcp", include_str!("../docs/mcp.md")),
    ("setup", include_str!("../docs/setup.md")),
    ("bundles", include_str!("../docs/bundles.md")),
];

/// Every doc, the CLI's first: `(key, text)`.
fn docs() -> impl Iterator<Item = (&'static str, &'static str)> {
    CLI_DOCS.iter().chain(fantastic_core::DOCS.iter()).copied()
}

/// A doc's one-line summary: its leading `> ` line.
fn summary(text: &str) -> &str {
    text.lines().next().and_then(|l| l.strip_prefix("> ")).unwrap_or("")
}

/// `fantastic readme` → the index; `fantastic readme <key>` → that doc. Reads
/// only what is embedded: nothing starts, nothing is written.
pub fn main(argv: &[String]) -> ! {
    match argv {
        [] => {
            print!("{}", index());
            std::process::exit(0);
        }
        [key] => match docs().find(|(k, _)| k == key) {
            Some((_, text)) => {
                print!("{text}");
                std::process::exit(0);
            }
            None => {
                eprintln!("fantastic readme: no doc `{key}`\n\n{}", index());
                std::process::exit(2);
            }
        },
        _ => {
            eprintln!("fantastic readme: one key at most (got {})", argv.len());
            std::process::exit(2);
        }
    }
}

/// The keys, each with its summary.
pub fn index() -> String {
    let w = docs().map(|(k, _)| k.len()).max().unwrap_or(0);
    let mut out = String::from("fantastic readme <key>:\n");
    for (k, t) in docs() {
        out.push_str(&format!("  {k:<w$}  {}\n", summary(t)));
    }
    out
}

/// The whole text: the commands, how to register the MCP face, what a
/// connected MCP client is told, and where the docs are.
pub fn text() -> String {
    format!("{COMMANDS}{SHAPE}{LEAD_IN}{INSTRUCTIONS}\n\nDetails: `fantastic readme` lists the docs, `fantastic readme <key>` prints one.\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_doc_has_a_summary_and_a_unique_key() {
        let mut keys = std::collections::HashSet::new();
        for (k, t) in docs() {
            assert!(!summary(t).is_empty(), "`{k}` opens with no `> ` summary");
            assert!(keys.insert(k), "`{k}` twice");
        }
        assert!(keys.contains("canvas") && keys.contains("mcp") && keys.contains("setup") && keys.contains("agents"));
    }

    #[test]
    fn the_index_lists_every_key() {
        let i = index();
        for (k, _) in docs() {
            assert!(i.contains(&format!("  {k} ")), "{k} missing from\n{i}");
        }
    }
}
