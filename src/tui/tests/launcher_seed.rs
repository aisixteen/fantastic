//! Issue #555: the launcher seeds before the kernel starts.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const STOCK: [&str; 5] = [
    ".fantastic/agents/user/agent.yaml",
    ".fantastic/agents/user/agents/exec/agent.yaml",
    ".fantastic/agents/user/agents/files/agent.yaml",
    ".fantastic/agents/user/agents/store/agent.yaml",
    ".fantastic/agents/user/agents/memory/agent.yaml",
];

fn fresh(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fc-launch-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn home(tag: &str) -> PathBuf {
    let d = fresh(&format!("home-{tag}"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    d
}

fn sv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn starts_kernel_is_public_and_bound_to_the_mode() {
    for a in [
        &["--headless"][..],
        &["canvas"],
        &["canvas", "--port", "8123"],
        &["mcp"],
    ] {
        assert!(fantastic_tui::starts_kernel(&sv(a)), "{a:?}");
    }
    for a in [
        &["send", "x"][..],
        &["import", "p", "--parent", "user"],
        &["migrate-home", "--dry-run"],
        &["__ctrl_break", "1"],
        &["mcp", "--can-call", "files"],
        &["bogus"],
    ] {
        assert!(!fantastic_tui::starts_kernel(&sv(a)), "{a:?}");
    }
}

#[test]
fn an_embedder_launcher_writes_its_blueprint_after_the_stock_one() {
    const EMB: &str = "agents:\n  - id: user\n    agents:\n      - id: probe\n        handler_module: ram_store.tools\n  - id: probe_top\n    handler_module: ram_store.tools\n";
    let dir = fresh("emb");
    let argv = vec!["canvas".to_string()];
    if fantastic_tui::starts_kernel(&argv) {
        assert_eq!(fantastic_tui::seed_dir_with(&dir.join(".fantastic"), &[EMB]), Ok(true));
    }
    for f in STOCK.iter().chain([
        &".fantastic/agents/user/agents/probe/agent.yaml",
        &".fantastic/agents/probe_top/agent.yaml",
    ]) {
        assert!(dir.join(f).is_file(), "missing {f}");
    }
    let dir2 = fresh("emb2");
    fantastic_tui::seed_dir(&dir2.join(".fantastic")).unwrap();
    let u = ".fantastic/agents/user/agent.yaml";
    assert_eq!(
        std::fs::read(dir.join(u)).unwrap(),
        std::fs::read(dir2.join(u)).unwrap()
    );

    let dir3 = fresh("emb3");
    let argv = vec!["send".to_string(), "x".to_string()];
    if fantastic_tui::starts_kernel(&argv) {
        fantastic_tui::seed_dir_with(&dir3.join(".fantastic"), &[EMB]).unwrap();
    }
    assert!(!dir3.join(".fantastic").exists());
    for d in [dir, dir2, dir3] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn a_one_shot_command_writes_nothing() {
    let cases: [(&[&str], i32); 3] = [
        (&[], 0),
        (&["mcp", "--can-call", "files"], 2),
        (&["bogus"], 2),
    ];
    for (i, (argv, code)) in cases.iter().enumerate() {
        let dir = fresh(&format!("one{i}"));
        let h = home(&format!("one{i}"));
        let out = Command::new(env!("CARGO_BIN_EXE_fantastic"))
            .args(*argv)
            .current_dir(&dir)
            .env("FANTASTIC_HOME", &h)
            .output()
            .expect("the fantastic binary runs");
        assert_eq!(
            out.status.code(),
            Some(*code),
            "{argv:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        if argv.is_empty() {
            assert!(!out.stdout.is_empty(), "the bare readme prints");
        }
        assert!(
            !dir.join(".fantastic").exists(),
            "{argv:?} wrote .fantastic"
        );
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(h);
    }
}

#[cfg(unix)]
#[test]
fn each_kernel_mode_seeds_a_fresh_folder_from_the_launcher() {
    for (i, kind) in ["mcp", "headless", "canvas"].iter().enumerate() {
        let argv: Vec<String> = match *kind {
            "mcp" => sv(&["mcp"]),
            "headless" => sv(&["--headless"]),
            _ => {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let p = l.local_addr().unwrap().port();
                drop(l);
                vec!["canvas".into(), "--port".into(), p.to_string()]
            }
        };
        let dir = fresh(&format!("k{i}"));
        let h = home(&format!("k{i}"));
        let mut child = Command::new(env!("CARGO_BIN_EXE_fantastic"))
            .args(&argv)
            .current_dir(&dir)
            .env("FANTASTIC_HOME", &h)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the fantastic binary starts");
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(60) && !STOCK.iter().all(|f| dir.join(f).is_file())
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
        for f in STOCK {
            assert!(dir.join(f).is_file(), "{argv:?}: missing {f}");
        }
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(h);
    }
}
