//! Issue #561: a launcher's readme sections follow the base readme.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

const SECTION: &str = "## an embedder's section\n\nprinted after the base readme\n";
const MARK: &str = "@@readme-launcher@@\n";

fn fresh(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fc-readme-{}-{tag}", std::process::id()));
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

/// Not a test of its own: the test binary re-execs itself as the launcher.
#[test]
fn readme_launcher_child() {
    if std::env::var("FC_README_LAUNCHER").is_err() {
        return;
    }
    let args: Vec<String> = vec![];
    print!("{MARK}");
    std::io::stdout().flush().unwrap();
    let _ = fantastic_tui::main_with(args, &[], &[SECTION]);
    std::io::stdout().flush().unwrap();
    std::process::exit(0);
}

#[test]
fn a_launcher_with_one_section_prints_the_base_then_the_section() {
    for mode in ["bare"] {
        let cwd = fresh(&format!("cwd-{mode}"));
        let h = home(mode);
        let mut base_cmd = Command::new(env!("CARGO_BIN_EXE_fantastic"));
        let base_out = base_cmd
            .current_dir(&cwd)
            .env("FANTASTIC_HOME", &h)
            .output()
            .unwrap();
        assert!(base_out.status.success(), "{mode}: base");
        let base = String::from_utf8(base_out.stdout).unwrap();

        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "readme_launcher_child",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("FC_README_LAUNCHER", mode)
            .current_dir(&cwd)
            .env("FANTASTIC_HOME", &h)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{mode}: child: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let out = String::from_utf8(child.stdout).unwrap();
        let (_, got) = out.split_once(MARK).expect("the launcher's mark");
        assert_eq!(got, format!("{base}\n{SECTION}"), "{mode}");
        assert!(!base.contains("printed after the base readme"));
        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_dir_all(&h);
    }
}
