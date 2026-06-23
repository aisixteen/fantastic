//! e2e CRUD for the AI connector config — create / read / update / delete across
//! `settings.json` (backend+model) + the keychain (key). Uses an in-memory secret
//! store so it NEVER touches the real OS keychain, and an isolated `FANTASTIC_HOME`
//! so it never touches the real settings file.

use fantastic_host::{ai_config, clear_ai_connector, hydrate_ai_env, secret, set_ai_connector};

#[test]
fn ai_connector_crud_roundtrip() {
    // Isolated home + in-memory keychain.
    let tmp = std::env::temp_dir().join(format!("ft-aicrud-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::env::set_var("FANTASTIC_HOME", &tmp);
    secret::use_store(Box::new(secret::MemStore::new()));

    // READ (empty): nothing configured yet.
    let c = ai_config();
    assert_eq!(c.backend, None);
    assert_eq!(c.model, None);
    assert!(!c.key_present);

    // CREATE: a connector with a key.
    set_ai_connector("nvidia", "model-a", Some("sk-secret")).expect("set connector");
    let c = ai_config();
    assert_eq!(c.backend.as_deref(), Some("nvidia"));
    assert_eq!(c.model.as_deref(), Some("model-a"));
    assert!(c.key_present, "key should be present after set");

    // The raw key lives ONLY in the keychain, NEVER in settings.json.
    assert_eq!(secret::get_key("nvidia").as_deref(), Some("sk-secret"));
    let raw = std::fs::read_to_string(fantastic_host::settings_path()).unwrap_or_default();
    assert!(
        !raw.contains("sk-secret"),
        "the key must NEVER appear in settings.json: {raw}"
    );

    // UPDATE: change the model + rotate the key.
    set_ai_connector("nvidia", "model-b", Some("sk-2")).expect("update connector");
    let c = ai_config();
    assert_eq!(c.model.as_deref(), Some("model-b"));
    assert_eq!(secret::get_key("nvidia").as_deref(), Some("sk-2"));

    // HYDRATE: the key flows keychain → `FANTASTIC_AI_KEY` (so `ensure_brain` sees it).
    for v in [
        "FANTASTIC_AI_KEY",
        "FANTASTIC_AI_BACKEND",
        "FANTASTIC_AI_MODEL",
    ] {
        std::env::remove_var(v);
    }
    hydrate_ai_env();
    assert_eq!(
        std::env::var("FANTASTIC_AI_BACKEND").ok().as_deref(),
        Some("nvidia")
    );
    assert_eq!(
        std::env::var("FANTASTIC_AI_KEY").ok().as_deref(),
        Some("sk-2")
    );

    // DELETE: connector + key gone.
    clear_ai_connector().expect("clear connector");
    let c = ai_config();
    assert_eq!(c.backend, None);
    assert!(!c.key_present);
    assert_eq!(secret::get_key("nvidia"), None);

    let _ = std::fs::remove_dir_all(&tmp);
}
