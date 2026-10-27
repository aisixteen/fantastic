//! The seed step is public: an embedding host can call it (issue #189).

#[test]
fn seed_dir_is_public_and_seeds() {
    let d = std::env::temp_dir().join(format!("fc-seed-pub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    assert_eq!(fantastic_tui::seed_dir(&d), Ok(true));
    assert!(d.join(".fantastic/agents/user/agent.yaml").is_file());
    assert_eq!(fantastic_tui::seed_dir(&d), Ok(false));
    let _ = std::fs::remove_dir_all(&d);
}
