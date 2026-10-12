//! THE COMMAND LINE:
//! `fantastic [--headless] [canvas [--port N]] [ai [BACKEND FLAGS…]] [run COMMAND [ARGS…]]`.
//!
//! A RECIPE is a word that names agents. It ADDS them to the kernel only if they
//! are missing — a recipe already applied adds nothing — and RUNS them. The
//! terminal UI is the default and is always there unless `--headless`; the
//! screen opens on the LAST recipe named (`fantastic canvas ai` runs both and
//! shows the ai). Every recipe works without `canvas` (the terminal UI is then
//! the only View).
//!
//! | recipe | adds (if missing) | shows |
//! |--------|-------------------|-------|
//! | `canvas` | `web` (the HTTP host), the canvas (`canvas_page`, `canvas_ws` + its helpers) | `web`, and serves the canvas |
//! | `ai` | with a BACKEND: the ROOT AI `ai` on it (`kern/root_ai`) — made, re-pointed, or remade on a new backend | the root AI, composer focused; with none, the set-up page |
//! | `run COMMAND [ARGS…]` | a terminal running exactly that command, from `$PATH`, as is (`kern/terminal`) — the user's own program at their discretion | the terminal (with `canvas`: a terminal cell) |
//!
//! `ai` BACKENDS (the word names the server it talks to):
//! - `vllm -p PORT [--host H] [--kv-overflow evict|reject]` — the rest is read from the server
//! - `ollama -p PORT [--host H]` — the rest is read from the server
//! - `responses --model M [--endpoint URL]` — the endpoint is OpenAI's Responses API unless named; the key is the one stored for it
//! - `anthropic --model M [--endpoint URL]` — the endpoint is Anthropic's Messages API unless named; a stored key is used when there is one
//!
//! `--host` is `127.0.0.1` and `--kv-overflow` `evict` unless named. `run` takes
//! EVERY word after it (the command's own flags included).
//!
//! `--headless`: no terminal UI; the kernel's message stream goes to stdout (it
//! may stand anywhere: `fantastic canvas --headless` is the same).
//! `--port N` belongs to `canvas`; a new one overwrites the recorded. The first
//! `canvas` without one picks a free port and records it; later runs reuse it. A kernel
//! already running here refuses every argument (`run.rs`). `fantastic mcp` stays its own
//! command (stdout is its protocol).

use crate::kern::root_ai::Backend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipe {
    Canvas,
    Ai,
    Run,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opts {
    pub recipes: Vec<Recipe>,
    pub port: Option<u16>,
    pub headless: bool,
    /// `ai BACKEND …`: the root AI's backend.
    pub ai: Option<Backend>,
    /// `run COMMAND [ARGS…]`: the argv, as typed.
    pub run: Option<Vec<String>>,
}

impl Opts {
    pub fn canvas(&self) -> bool {
        self.recipes.contains(&Recipe::Canvas)
    }

    /// What the screen opens on: the last recipe named.
    pub fn shows(&self) -> Option<Recipe> {
        self.recipes.last().copied()
    }
}

const USAGE: &str = "run `fantastic [--headless] [canvas [--port N]] [ai [vllm|ollama|responses|anthropic …]] [run COMMAND …]`, `fantastic mcp`, `fantastic send <item>` or `fantastic readme [key]`";

fn port_of(v: &str, flag: &str) -> Result<u16, String> {
    v.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| format!("{flag} must be 1..65535, got `{v}`"))
}

/// The flags after `ai BACKEND`, up to the next recipe word.
#[derive(Default)]
struct AiFlags {
    port: Option<u16>,
    host: Option<String>,
    overflow: Option<String>,
    model: Option<String>,
    endpoint: Option<String>,
}

fn backend(word: &str, f: AiFlags) -> Result<Backend, String> {
    let host = f.host.clone().unwrap_or_else(|| "127.0.0.1".into());
    let only = |allowed: &[&str]| -> Result<(), String> {
        let given = [("-p", f.port.is_some()), ("--host", f.host.is_some()), ("--kv-overflow", f.overflow.is_some()),
                     ("--model", f.model.is_some()), ("--endpoint", f.endpoint.is_some())];
        match given.iter().find(|(n, set)| *set && !allowed.contains(n)) {
            Some((n, _)) => Err(format!("`ai {word}` does not take {n}")),
            None => Ok(()),
        }
    };
    match word {
        "vllm" => {
            only(&["-p", "--host", "--kv-overflow"])?;
            let port = f.port.ok_or("`ai vllm` needs -p <the server's port>")?;
            Ok(Backend::Vllm { host, port, kv_overflow: f.overflow.unwrap_or_else(|| "evict".into()) })
        }
        "ollama" => {
            only(&["-p", "--host"])?;
            let port = f.port.ok_or("`ai ollama` needs -p <the server's port>")?;
            Ok(Backend::Ollama { host, port })
        }
        "responses" => {
            only(&["--model", "--endpoint"])?;
            let model = f.model.ok_or("`ai responses` needs --model <the model name>")?;
            Ok(Backend::Responses { endpoint: f.endpoint.unwrap_or_else(|| crate::kern::root_ai::RESPONSES_ENDPOINT.into()), model })
        }
        "anthropic" => {
            only(&["--model", "--endpoint"])?;
            let model = f.model.ok_or("`ai anthropic` needs --model <the model name>")?;
            Ok(Backend::Anthropic { endpoint: f.endpoint.unwrap_or_else(|| crate::kern::root_ai::ANTHROPIC_ENDPOINT.into()), model })
        }
        other => Err(format!("`ai` takes vllm, ollama, responses or anthropic, got `{other}`")),
    }
}

pub fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { recipes: Vec::new(), port: None, headless: false, ai: None, run: None };
    let mut i = 0;
    let next = |i: &mut usize, what: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i).filter(|v| !v.is_empty()).cloned().ok_or_else(|| format!("{what}"))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--headless" => o.headless = true,
            "--port" => o.port = Some(port_of(&next(&mut i, "--port needs a number")?, "--port")?),
            "canvas" => push(&mut o.recipes, Recipe::Canvas),
            "ai" => {
                push(&mut o.recipes, Recipe::Ai);
                let word = match args.get(i + 1).map(String::as_str) {
                    Some(w @ ("vllm" | "ollama" | "responses" | "anthropic")) => w.to_string(),
                    Some(w) if !w.starts_with('-') && !matches!(w, "canvas" | "run" | "ai") => {
                        return Err(format!("`ai` takes vllm, ollama, responses or anthropic, got `{w}`"))
                    }
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                i += 1;
                let mut f = AiFlags::default();
                while let Some(flag) = args.get(i + 1).map(String::as_str) {
                    match flag {
                        "-p" => {
                            i += 1;
                            f.port = Some(port_of(&next(&mut i, "-p needs the server's port")?, "-p")?);
                        }
                        "--host" => {
                            i += 1;
                            f.host = Some(next(&mut i, "--host needs the server's host")?);
                        }
                        "--kv-overflow" => {
                            i += 1;
                            match next(&mut i, "--kv-overflow is evict or reject")?.as_str() {
                                k @ ("evict" | "reject") => f.overflow = Some(k.to_string()),
                                other => return Err(format!("--kv-overflow is evict or reject, got `{other}`")),
                            }
                        }
                        "--model" => {
                            i += 1;
                            f.model = Some(next(&mut i, "--model needs the model name")?);
                        }
                        "--endpoint" => {
                            i += 1;
                            f.endpoint = Some(next(&mut i, "--endpoint needs a URL")?);
                        }
                        _ => break,
                    }
                }
                o.ai = Some(backend(&word, f)?);
            }
            "run" => {
                push(&mut o.recipes, Recipe::Run);
                let argv: Vec<String> = args[i + 1..].to_vec();
                if argv.is_empty() {
                    return Err("`run` needs the command to run".into());
                }
                o.run = Some(argv);
                break;
            }
            other => return Err(format!("unknown `{other}` — {USAGE}")),
        }
        i += 1;
    }
    if o.port.is_some() && !o.canvas() {
        return Err("--port belongs to the `canvas` recipe".into());
    }
    if o.headless && o.run.is_some() && !o.canvas() {
        return Err("`run` needs a View: the terminal UI (drop --headless) or the canvas".into());
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
        let o = p("canvas ai").unwrap();
        assert_eq!(o.recipes, vec![Recipe::Canvas, Recipe::Ai]);
        assert_eq!(o.shows(), Some(Recipe::Ai));
        assert!(o.canvas() && !o.headless && o.ai.is_none());
        assert!(p("web").is_err(), "`web` is no longer a word");
        assert_eq!(p("ai canvas").unwrap().shows(), Some(Recipe::Canvas));
        assert_eq!(p("canvas ai canvas").unwrap().recipes, vec![Recipe::Ai, Recipe::Canvas], "named twice runs once, last");
    }

    #[test]
    fn the_terminal_is_the_default_and_tui_is_no_longer_a_word() {
        let o = p("").unwrap();
        assert!(o.recipes.is_empty() && !o.headless);
        assert!(p("tui").is_err());
    }

    #[test]
    fn port_belongs_to_canvas() {
        assert_eq!(p("canvas --port 8123").unwrap().port, Some(8123));
        assert!(p("canvas --headless").unwrap().headless, "a flag after the recipe counts the same");
        assert!(p("canvas --headless").unwrap().port.is_none(), "no port: the first run picks one");
        assert!(p("ai --port 8123").is_err());
        assert!(p("canvas --port 0").is_err());
    }

    #[test]
    fn ai_backends_with_or_without_canvas() {
        let v = p("ai vllm -p 10000").unwrap();
        assert!(!v.canvas());
        assert_eq!(v.ai, Some(Backend::Vllm { host: "127.0.0.1".into(), port: 10000, kv_overflow: "evict".into() }));
        assert_eq!(
            p("canvas ai vllm -p 1 --host h --kv-overflow reject").unwrap().ai,
            Some(Backend::Vllm { host: "h".into(), port: 1, kv_overflow: "reject".into() })
        );
        assert_eq!(p("ai ollama -p 11434").unwrap().ai, Some(Backend::Ollama { host: "127.0.0.1".into(), port: 11434 }));
        assert_eq!(
            p("ai responses --model m").unwrap().ai,
            Some(Backend::Responses { endpoint: crate::kern::root_ai::RESPONSES_ENDPOINT.into(), model: "m".into() })
        );
        assert_eq!(
            p("ai responses --model m --endpoint https://x/v1/responses").unwrap().ai,
            Some(Backend::Responses { endpoint: "https://x/v1/responses".into(), model: "m".into() })
        );
        assert!(p("ai vllm").is_err(), "no port is refused, never guessed");
        assert!(p("ai responses").is_err(), "no model is refused");
        assert_eq!(
            p("ai anthropic --model m").unwrap().ai,
            Some(Backend::Anthropic { endpoint: crate::kern::root_ai::ANTHROPIC_ENDPOINT.into(), model: "m".into() })
        );
        assert!(p("ai anthropic").is_err(), "no model is refused");
        assert!(p("ai openai").is_err());
        assert!(p("ai vllm -p 1 --model m").is_err(), "a flag the backend does not take is refused");
        assert!(p("ai vllm -p 1 --kv-overflow drop").is_err());
        assert_eq!(p("ai vllm -p 1 canvas").unwrap().recipes, vec![Recipe::Ai, Recipe::Canvas], "flags end at the next recipe");
    }

    #[test]
    fn run_takes_every_word_after_it() {
        let o = p("run claude --resume x").unwrap();
        assert_eq!(o.run, Some(vec!["claude".into(), "--resume".into(), "x".into()]));
        assert_eq!(o.shows(), Some(Recipe::Run));
        assert_eq!(p("canvas run htop").unwrap().recipes, vec![Recipe::Canvas, Recipe::Run]);
        assert!(p("run").is_err());
        assert!(p("--headless run htop").is_err(), "headless with no canvas has no View for it");
        assert!(p("--headless canvas run htop").is_ok());
    }
}
