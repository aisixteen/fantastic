//! THE README a bare `fantastic` prints when it is NOT on a terminal (owner,
//! 2026-10-01: "in non-tui do not run w/o flags, just show readme"). A coding
//! CLI that runs `fantastic` in its shell reads what this is and how to drive
//! it; nothing is written to the directory and no kernel starts. On a terminal
//! the same command opens the terminal UI. An embedding launcher's readme
//! sections (passed to `main_with`) are printed after this text, in order; the
//! same for `fantastic readme`. What an agent is and how to call it is never written
//! here: it is rendered from the agents' own declarations (`reflect`, `docs`).

use fantastic_web_mcp::{INSTRUCTIONS, LEAD_IN, SHAPE};

const COMMANDS: &str = "Aisixteen Fantastic — a kernel of agents with a state folder (default `.fantastic/` in the work folder), driven by one verb, send, and discovered by reflect.\n\
\n\
This is `fantastic` without a terminal, so it only prints this readme: it starts nothing and writes nothing. Commands:\n\
\x20\x20fantastic                          the terminal UI (on a terminal); a fresh state folder is seeded on launch\n\
\x20\x20fantastic --statedir DIR --workdir DIR  on the UI, `--headless`, `canvas` and `mcp`: the state folder (default `<workdir>/.fantastic`) and the work folder (default the current dir; the root record keeps it, and a running kernel refuses a change)\n\
\x20\x20fantastic canvas [ai] --port N     the browser canvas, headless: the kernel's message stream on stdout (--port the first time only)\n\
\x20\x20fantastic [canvas] ai vllm -p PORT  the root AI `ai` on a backend (also `ai ollama -p PORT`, `ai responses|anthropic --model M`)\n\
\x20\x20fantastic [canvas] run CMD [ARGS]   your own program from $PATH, as is, in the terminal (or a canvas cell)\n\
\x20\x20fantastic --headless               no screen: the kernel's message stream on stdout; a recorded `web` host still serves its routes\n\
\x20\x20fantastic mcp [--statedir DIR] [--workdir DIR]  MCP over stdio for a coding CLI (attaches to a running kernel, or runs it)\n\
\x20\x20fantastic send [--statedir DIR] '<item json>'  one call into the running kernel, as the `mcp` leg; `$env:NAME` values come from this shell, marked secret\n\
\x20\x20fantastic import PATH --parent ID  import a blueprint folder as agents under ID: prints the preview, then asks (`--bind @port=ID`, `--allow-code`, `--approve` skips the question)\n\
\x20\x20fantastic migrate-home [--dry-run]  move the files of an older layout (~/.config/fantastic, ~/.fantastic/credentials) into ~/.fantastic; --dry-run only prints the moves\n\
\x20\x20fantastic readme                  this text\n\
\n";

/// `base`, then each section in order, set off by a blank line, each ended by a newline.
fn with_sections(base: String, sections: &[&str]) -> String {
    let mut out = base;
    for s in sections {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(s);
        if !s.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// `fantastic readme` → the readme text, then each embedder section. Reads only
/// what is embedded: nothing starts, nothing is written.
pub fn main(argv: &[String], sections: &[&str]) -> ! {
    if !argv.is_empty() {
        eprintln!("fantastic readme: takes no argument (got {})", argv.len());
        std::process::exit(2);
    }
    print!("{}", text(sections));
    std::process::exit(0);
}

/// The whole text: the commands, how to register the MCP face and what a
/// connected MCP client is told, then each embedder section the launcher passed
/// (none for `fantastic`).
pub fn text(sections: &[&str]) -> String {
    with_sections(
        format!("{COMMANDS}{SHAPE}{LEAD_IN}{INSTRUCTIONS}\n\nDetails: `reflect` an agent for what it is and `docs` for how to call it — both are rendered from what the agent declares.\n"),
        sections,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_readme_with_no_sections_is_the_base_text() {
        let t = text(&[]);
        assert!(t.starts_with(COMMANDS));
        assert!(t.ends_with("both are rendered from what the agent declares.\n"));
    }

    #[test]
    fn one_section_follows_the_base_text() {
        const S: &str = "## extra\n\nan embedder's own section\n";
        assert_eq!(text(&[S]), format!("{}\n{S}", text(&[])));
    }

    #[test]
    fn sections_follow_in_order_each_ended_by_a_newline() {
        assert_eq!(text(&["A\n", "B"]), format!("{}\nA\n\nB\n", text(&[])));
    }
}
