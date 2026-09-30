//! THE COMMAND LINE: `fantastic [--headless] [--port N] [recipe…]`.
//!
//! A RECIPE is a word that names agents. It ADDS them to the kernel only if they
//! are missing — a recipe already applied adds nothing — and RUNS them. The
//! terminal UI is the default and is always there unless `--headless`; the
//! screen opens on the LAST recipe named (`fantastic web ai` runs both and
//! shows the ai).
//!
//! | recipe | adds (if missing)                     | shows                       |
//! |--------|---------------------------------------|-----------------------------|
//! | `web`  | `ui`, `web`, `web_ui`, `ui_assets`    | `web`, and serves the page  |
//! | `ai`   | nothing yet (model/grants/brief: TBD) | the ai agent, composer focused; with none, the set-up page |
//!
//! `--headless`: no terminal UI; the kernel's message stream goes to stdout.
//! `--port N` belongs to `web`, creation-only. `fantastic mcp` stays its own
//! command (stdout is its protocol).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipe {
    Web,
    Ai,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opts {
    pub recipes: Vec<Recipe>,
    pub port: Option<u16>,
    pub headless: bool,
}

impl Opts {
    pub fn web(&self) -> bool {
        self.recipes.contains(&Recipe::Web)
    }

    /// What the screen opens on: the last recipe named.
    pub fn shows(&self) -> Option<Recipe> {
        self.recipes.last().copied()
    }

    /// A recipe or `--headless` is the command the human typed, and like `mcp`
    /// it is an explicit trigger for init. Bare `fantastic` inits on the intro's
    /// key press.
    pub fn inits_on_command(&self) -> bool {
        self.headless || !self.recipes.is_empty()
    }
}

pub fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { recipes: Vec::new(), port: None, headless: false };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--headless" => o.headless = true,
            "--port" => {
                let v = it.next().ok_or("--port needs a number")?;
                o.port = Some(
                    v.parse::<u16>()
                        .ok()
                        .filter(|p| *p > 0)
                        .ok_or_else(|| format!("--port must be 1..65535, got `{v}`"))?,
                );
            }
            "web" => push(&mut o.recipes, Recipe::Web),
            "ai" => push(&mut o.recipes, Recipe::Ai),
            other => {
                return Err(format!(
                    "unknown `{other}` — run `fantastic [--headless] [--port N] [web] [ai]`, or `fantastic mcp`"
                ))
            }
        }
    }
    if o.port.is_some() && !o.web() {
        return Err("--port belongs to the `web` recipe".into());
    }
    Ok(o)
}

/// A recipe named twice runs once, at its LAST position (that is what shows).
fn push(v: &mut Vec<Recipe>, r: Recipe) {
    v.retain(|x| *x != r);
    v.push(r);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Opts, String> {
        parse(&s.split_whitespace().map(str::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn recipes_run_in_order_and_the_last_one_shows() {
        let o = p("web ai").unwrap();
        assert_eq!(o.recipes, vec![Recipe::Web, Recipe::Ai]);
        assert_eq!(o.shows(), Some(Recipe::Ai));
        assert!(o.web() && !o.headless);
        assert_eq!(p("ai web").unwrap().shows(), Some(Recipe::Web));
        assert_eq!(p("web ai web").unwrap().recipes, vec![Recipe::Ai, Recipe::Web], "named twice runs once, last");
    }

    #[test]
    fn the_terminal_is_the_default_and_tui_is_no_longer_a_word() {
        let o = p("").unwrap();
        assert!(o.recipes.is_empty() && !o.headless && !o.inits_on_command());
        assert!(p("tui").is_err());
        assert!(p("--headless").unwrap().inits_on_command());
    }

    #[test]
    fn port_belongs_to_web() {
        assert_eq!(p("web --port 8123").unwrap().port, Some(8123));
        assert!(p("ai --port 8123").is_err());
        assert!(p("web --port 0").is_err());
    }
}
