//! THE COMMAND LINE: `fantastic [--headless] [--port N] [recipe…]`.
//!
//! A RECIPE is a word that names agents. It ADDS them to the kernel only if they
//! are missing — a recipe already applied adds nothing — and RUNS them. The
//! terminal UI is the default and is always there unless `--headless`; the
//! screen opens on the LAST recipe named (`fantastic canvas ai` runs both and
//! shows the ai).
//!
//! | recipe | adds (if missing)                     | shows                       |
//! |--------|---------------------------------------|-----------------------------|
//! | `canvas` | `web` (the HTTP host), the canvas (`canvas_page`, `canvas_ws` + its helpers) | `web`, and serves the canvas |
//! | `ai`   | nothing yet (model/grants/brief: TBD) | the ai agent, composer focused; with none, the set-up page |
//!
//! `canvas run vllm -p PORT [--host HOST] [--kv-overflow evict|reject]`: the
//! canvas's root AI (its main chat's) speaks to that vLLM server — made, or
//! re-pointed; the rest of its config is read from the server (`kern/root_ai`).
//! `--host` is `127.0.0.1` and `--kv-overflow` `evict` unless named.
//!
//! `--headless`: no terminal UI; the kernel's message stream goes to stdout.
//! `--port N` belongs to `canvas`, creation-only. `fantastic mcp` stays its own
//! command (stdout is its protocol).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipe {
    Canvas,
    Ai,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opts {
    pub recipes: Vec<Recipe>,
    pub port: Option<u16>,
    pub headless: bool,
    pub vllm: Option<crate::kern::root_ai::VllmRun>,
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

pub fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { recipes: Vec::new(), port: None, headless: false, vllm: None };
    let (mut vport, mut vhost, mut overflow, mut run) = (None, None, None, false);
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
            "canvas" => push(&mut o.recipes, Recipe::Canvas),
            "ai" => push(&mut o.recipes, Recipe::Ai),
            "run" => {
                match it.next().map(String::as_str) {
                    Some("vllm") => run = true,
                    other => return Err(format!("`run` takes `vllm`, got `{}`", other.unwrap_or(""))),
                }
            }
            "-p" => {
                let v = it.next().ok_or("-p needs the vLLM server's port")?;
                vport = Some(v.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(|| format!("-p must be 1..65535, got `{v}`"))?);
            }
            "--host" => vhost = Some(it.next().filter(|h| !h.is_empty()).ok_or("--host needs the vLLM server's host")?.clone()),
            "--kv-overflow" => match it.next().map(String::as_str) {
                Some(k @ ("evict" | "reject")) => overflow = Some(k.to_string()),
                other => return Err(format!("--kv-overflow is evict or reject, got `{}`", other.unwrap_or(""))),
            },
            other => {
                return Err(format!(
                    "unknown `{other}` — run `fantastic [--headless] [--port N] [canvas] [ai]`, `fantastic canvas run vllm -p PORT [--host HOST]`, `fantastic mcp`, `fantastic send <item>` or `fantastic readme [key]`"
                ))
            }
        }
    }
    if o.port.is_some() && !o.canvas() {
        return Err("--port belongs to the `canvas` recipe".into());
    }
    if run {
        if !o.canvas() {
            return Err("`run vllm` belongs to the `canvas` recipe".into());
        }
        let port = vport.ok_or("`run vllm` needs -p <the vLLM server's port>")?;
        let host = vhost.unwrap_or_else(|| "127.0.0.1".into());
        o.vllm = Some(crate::kern::root_ai::VllmRun { host, port, kv_overflow: overflow.unwrap_or_else(|| "evict".into()) });
    } else if vport.is_some() || vhost.is_some() || overflow.is_some() {
        return Err("-p, --host and --kv-overflow belong to `canvas run vllm`".into());
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
        assert!(o.canvas() && !o.headless);
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
        assert!(p("ai --port 8123").is_err());
        assert!(p("canvas --port 0").is_err());
    }

    #[test]
    fn run_vllm_needs_a_port_and_defaults_host_and_overflow() {
        let o = p("canvas run vllm -p 10000 --host 127.0.0.1").unwrap();
        let v = o.vllm.unwrap();
        assert_eq!((v.host.as_str(), v.port, v.kv_overflow.as_str()), ("127.0.0.1", 10000, "evict"));
        assert_eq!(v.endpoint(), "http://127.0.0.1:10000/v1");
        assert_eq!(p("canvas run vllm -p 1 --host h --kv-overflow reject").unwrap().vllm.unwrap().kv_overflow, "reject");
        assert_eq!(p("canvas run vllm -p 10000").unwrap().vllm.unwrap().host, "127.0.0.1", "the host is local unless named");
        assert!(p("canvas run vllm --host h").is_err(), "no port is refused");
        assert!(p("run vllm -p 1 --host h").is_err(), "belongs to canvas");
        assert!(p("canvas run ollama").is_err());
        assert!(p("canvas -p 1").is_err());
        assert!(p("canvas run vllm -p 1 --host h --kv-overflow drop").is_err());
    }
}
