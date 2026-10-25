//! `fantastic import <path> --parent <id> [--bind @port=id]… [--allow-code] [--approve]`
//! — the `user` agent's `import_subtree` verb from a shell (#269).
//!
//! - ATTACH-ONLY, like `send`: one MCP call over the running kernel's socket;
//!   no live kernel → it fails loud and starts nothing.
//! - Without `--approve` it prints the PREVIEW (the new ids, what each reaches,
//!   the ports, the code) and creates nothing. On a terminal it then asks
//!   `approve? [y/N]` and on `y` sends the same call again with `approve` true;
//!   off a terminal it never prompts.
//! - Exit 0 for a preview or an import, 1 for a refusal, 2 for a bad command line.

use std::io::{BufRead as _, IsTerminal as _, Write as _};

use serde_json::{json, Map, Value};

use crate::env::Paths;

/// What the command line said.
#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub path: String,
    pub parent: String,
    pub bind: Vec<(String, String)>,
    pub allow_code: bool,
    pub approve: bool,
}

const USAGE: &str = "fantastic import <path> --parent <agent id> [--bind @port=agent_id]… [--allow-code] [--approve]";

/// Read the arguments after `import`.
pub fn parse(argv: &[String]) -> Result<Args, String> {
    let (mut path, mut parent) = (None, None);
    let (mut bind, mut allow_code, mut approve) = (Vec::new(), false, false);
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--parent" => {
                parent = Some(it.next().ok_or("--parent needs an agent id")?.clone());
            }
            "--bind" => {
                let v = it.next().ok_or("--bind needs @port=agent_id")?;
                let (port, id) = v
                    .split_once('=')
                    .filter(|(p, i)| !p.is_empty() && !i.is_empty())
                    .ok_or_else(|| format!("--bind {v}: expected @port=agent_id"))?;
                bind.push((port.to_string(), id.to_string()));
            }
            "--allow-code" => allow_code = true,
            "--approve" => approve = true,
            f if f.starts_with("--") => return Err(format!("unknown flag {f}")),
            _ if path.is_some() => return Err(format!("one path only (also got {a})")),
            _ => path = Some(a.clone()),
        }
    }
    Ok(Args {
        path: path.ok_or("no path: the blueprint folder to import")?,
        parent: parent.ok_or("--parent <agent id> is required: the agent to import under")?,
        bind,
        allow_code,
        approve,
    })
}

/// The `send` item: `allow_code` and `approve` are the literal `true` or absent.
pub fn item(a: &Args, approve: bool) -> Value {
    let bind: Map<String, Value> = a.bind.iter().map(|(p, i)| (p.clone(), json!(i))).collect();
    let mut payload = json!({"type": "import_subtree", "path": a.path,
        "parent_id": a.parent, "bind": bind});
    if a.allow_code {
        payload["allow_code"] = json!(true);
    }
    if approve {
        payload["approve"] = json!(true);
    }
    json!({"target": "user", "payload": payload})
}

fn names(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// The verb's reply, as lines for a person.
pub fn render(reply: &Value) -> String {
    let mut out = String::new();
    if let Some(e) = reply.get("error").and_then(Value::as_str) {
        let reason = reply
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("error");
        return format!("refused ({reason}): {e}\n");
    }
    if reply.get("approval_required") == Some(&json!(true)) {
        let ids: Vec<String> = reply["new_ids"]
            .as_object()
            .map(|m| {
                m.values()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        out += &format!(
            "would create {} agents: {}\n",
            reply["agents"],
            ids.join(", ")
        );
        for r in reply["reaches"].as_array().into_iter().flatten() {
            let mut parts = Vec::new();
            for (field, word) in [("can_call", "calls"), ("callable_by", "called by")] {
                let n = names(&r[field]);
                if !n.is_empty() {
                    parts.push(format!("{word} {n}"));
                }
            }
            let reaches = if parts.is_empty() {
                "nothing outside".into()
            } else {
                parts.join("; ")
            };
            out += &format!(
                "  {} (from {}): reaches {reaches}\n",
                r["id"].as_str().unwrap_or("?"),
                r["from"].as_str().unwrap_or("?")
            );
        }
        match reply["ports"].as_object().filter(|m| !m.is_empty()) {
            Some(m) => {
                let b: Vec<String> = m
                    .iter()
                    .map(|(p, i)| format!("{p} = {}", i.as_str().unwrap_or("?")))
                    .collect();
                out += &format!("ports: {}\n", b.join(", "));
            }
            None => out += "ports: none\n",
        }
        match reply["code"].as_array().filter(|c| !c.is_empty()) {
            Some(c) => {
                out += "code:\n";
                for l in c {
                    out += &format!(
                        "  {}\n",
                        l.as_str().map_or_else(|| l.to_string(), String::from)
                    );
                }
            }
            None => out += "no code\n",
        }
        if let Some(h) = reply.get("hint").and_then(Value::as_str) {
            out += &format!("{h}\n");
        }
        return out;
    }
    if reply.get("imported").is_some() {
        return format!("imported {}: {}\n", reply["imported"], names(&reply["ids"]));
    }
    format!("{reply}\n")
}

fn die(code: i32, msg: impl std::fmt::Display) -> ! {
    eprintln!("fantastic import: {msg}");
    std::process::exit(code);
}

/// Send one item, the verb's reply back (an error text when the call itself failed).
fn send(rt: &tokio::runtime::Runtime, item: Value) -> Result<Value, String> {
    let row = rt.block_on(crate::send::call(item))?;
    if row.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(row.to_string());
    }
    Ok(row.get("result").cloned().unwrap_or(Value::Null))
}

/// Entry point, called from `main` before any runtime exists.
pub fn main(argv: &[String]) -> ! {
    let a = parse(argv).unwrap_or_else(|e| die(2, format!("{e}\nusage: {USAGE}")));
    let paths = Paths::here().unwrap_or_else(|e| die(2, format!("no working directory: {e}")));
    match fantastic_kernel::lock::held(&paths.cwd) {
        Ok(Some(pid)) if pid != std::process::id() => {}
        Ok(_) => die(
            1,
            "no kernel runs in this directory — start `fantastic` or `fantastic mcp` here first",
        ),
        Err(e) => die(1, e),
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| die(1, format!("no runtime: {e}")));
    let mut reply = send(&rt, item(&a, a.approve)).unwrap_or_else(|e| die(1, e));
    print!("{}", render(&reply));
    let preview = reply.get("approval_required") == Some(&json!(true));
    if preview && !a.approve && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        print!("approve? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        if matches!(line.trim(), "y" | "Y" | "yes") {
            reply = send(&rt, item(&a, true)).unwrap_or_else(|e| die(1, e));
            print!("{}", render(&reply));
        }
    }
    std::process::exit(i32::from(reply.get("error").is_some()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn args() -> Args {
        parse(&argv(&["bp", "--parent", "home", "--bind", "@memory=m1"])).unwrap()
    }

    #[test]
    fn parse_reads_path_parent_bind_and_flags() {
        let a = parse(&argv(&[
            "folder/bp",
            "--parent",
            "home",
            "--bind",
            "@memory=m1",
            "--bind",
            "@exec=x=y",
            "--allow-code",
            "--approve",
        ]))
        .unwrap();
        assert_eq!(a.path, "folder/bp");
        assert_eq!(a.parent, "home");
        assert_eq!(
            a.bind,
            vec![
                ("@memory".into(), "m1".into()),
                ("@exec".into(), "x=y".into())
            ]
        );
        assert!(a.allow_code && a.approve);
        let b = args();
        assert!(!b.allow_code && !b.approve);
    }

    #[test]
    fn parse_refuses_missing_parent_bad_bind_and_unknown_flag() {
        let e = parse(&argv(&["bp"])).unwrap_err();
        assert!(e.contains("--parent"), "{e}");
        let e = parse(&argv(&["--parent", "h"])).unwrap_err();
        assert!(e.contains("path"), "{e}");
        for bad in ["@memory", "@memory=", "=m1"] {
            let e = parse(&argv(&["bp", "--parent", "h", "--bind", bad])).unwrap_err();
            assert!(e.contains(bad) && e.contains("--bind"), "{e}");
        }
        let e = parse(&argv(&["bp", "--parent", "h", "--frobnicate"])).unwrap_err();
        assert!(e.contains("--frobnicate"), "{e}");
        let e = parse(&argv(&["bp", "other", "--parent", "h"])).unwrap_err();
        assert!(e.contains("other"), "{e}");
    }

    #[test]
    fn item_carries_allow_code_and_approve_only_when_asked() {
        let a = args();
        let it = item(&a, false);
        assert_eq!(it["target"], "user");
        let p = it["payload"].as_object().unwrap();
        assert_eq!(p["type"], "import_subtree");
        assert_eq!(p["path"], "bp");
        assert_eq!(p["parent_id"], "home");
        assert_eq!(p["bind"], json!({"@memory": "m1"}));
        assert!(!p.contains_key("approve") && !p.contains_key("allow_code"));
        let mut c = a;
        c.allow_code = true;
        let p = item(&c, true)["payload"].clone();
        assert_eq!(p["approve"], json!(true));
        assert_eq!(p["allow_code"], json!(true));
    }

    #[test]
    fn render_preview_lists_ids_reaches_ports_code_and_the_hint() {
        let r = json!({"approval_required": true, "agents": 2,
            "new_ids": {"a": "a_1", "b": "b_1"},
            "reaches": [{"id": "a_1", "from": "a", "can_call": ["m1"], "callable_by": [], "ports": {"@memory": "m1"}},
                        {"id": "b_1", "from": "b"}],
            "ports": {"@memory": "m1"}, "code": ["b_1: exec_bridge.tools"],
            "hint": "nothing was created: call again with approve: true to import"});
        let s = render(&r);
        for want in [
            "a_1, b_1",
            "a_1 (from a): reaches calls m1",
            "b_1 (from b): reaches nothing outside",
            "@memory = m1",
            "b_1: exec_bridge.tools",
            "nothing was created",
        ] {
            assert!(s.contains(want), "{want}: {s}");
        }
        let none = render(
            &json!({"approval_required": true, "agents": 0, "new_ids": {}, "reaches": [], "ports": {}, "code": []}),
        );
        assert!(
            none.contains("no code") && none.contains("ports: none"),
            "{none}"
        );
    }

    #[test]
    fn render_refusal_prints_reason_and_the_message_naming_the_fix() {
        let s = render(&json!({"reason": "unbound_ports",
            "error": "unbound required ports: @memory (memory): pass bind {\"@memory\": \"<agent id>\"}"}));
        assert!(
            s.starts_with("refused (unbound_ports): ") && s.contains("pass bind"),
            "{s}"
        );
        let s = render(
            &json!({"reason": "code_not_allowed", "error": "the blueprint carries code: pass allow_code: true"}),
        );
        assert!(
            s.contains("refused (code_not_allowed)") && s.contains("allow_code"),
            "{s}"
        );
    }

    #[test]
    fn render_imported_lists_the_new_ids() {
        let s = render(
            &json!({"imported": 2, "ids": ["a_1", "b_1"], "parent_id": "home", "ports": {}}),
        );
        assert!(s.contains("imported 2") && s.contains("a_1, b_1"), "{s}");
    }
}
