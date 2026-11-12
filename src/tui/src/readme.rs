//! THE README a bare `fantastic` prints when it is NOT on a terminal (owner,
//! 2026-10-01: "in non-tui do not run w/o flags, just show readme"). A coding
//! CLI that runs `fantastic` in its shell reads what this is and how to drive
//! it; nothing is written to the directory and no kernel starts. On a terminal
//! the same command opens the terminal UI. An embedding launcher's readme
//! sections (passed to `main_with`) are printed after this text, in order. What an agent is and how to call it is never written
//! here: it is rendered from the agents' own declarations (`reflect`, `docs`).

use fantastic_web_mcp::INSTRUCTIONS;

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

/// The whole text: what a connected MCP client is told, then each embedder section the launcher passed
/// (none for `fantastic`).
pub fn text(sections: &[&str]) -> String {
    with_sections(
        format!("{INSTRUCTIONS}\n"),
        sections,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_readme_with_no_sections_is_the_base_text() {
        let t = text(&[]);
        assert_eq!(t, format!("{INSTRUCTIONS}\n"));
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
