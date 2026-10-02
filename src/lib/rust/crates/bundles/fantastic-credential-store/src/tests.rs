//! credential_store: secrets by ref, bound to a host, read only with a secret
//! mark, behind per-ref grants — the twin of python
//! `credential_store/tests/test_credential_store.py`. Tests never touch the real
//! keychain or home: the file backend writes under a temp dir, the keychain
//! backend runs on an in-memory keychain.

use super::*;
use crate::keychain::MemoryKeychain;
use fantastic_kernel::agent::Agent;
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::BundleRegistry;
use fantastic_kernel::test_support::register_root_loader;
use fantastic_kernel::{with_sender, Item, Outcome};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const KEY: &str = "sk-live-0123456789abcdef";
const KEY2: &str = "sk-live-rotated-fedcba98";

#[test]
fn readme_is_python_verbatim() {
    let py = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .expect("repo's src/lib above the crate")
        .join("python/bundled_agents/security/credential_store/src/credential_store/readme.md");
    assert_eq!(README, std::fs::read_to_string(&py).expect("python readme"));
}

struct Fixture {
    k: Arc<Kernel>,
    keychain: Arc<MemoryKeychain>,
    tmp: tempfile::TempDir,
}

impl Fixture {
    fn file(&self) -> std::path::PathBuf {
        self.tmp.path().join("creds.yaml")
    }
}

/// python `_mk`: a kernel with a guid, four plain agents granted `creds`, and the
/// store (file under the temp dir) whose `callable_by` names them.
async fn mk(meta: Value) -> Fixture {
    let keychain = Arc::new(MemoryKeychain::default());
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(
        fantastic_ram_store::HANDLER_MODULE,
        fantastic_ram_store::RamStoreBundle::default(),
    );
    reg.register(
        HANDLER_MODULE,
        CredentialStoreBundle::with_keychain(Arc::clone(&keychain) as Arc<dyn Keychain>),
    );
    let k = bootstrap(reg, BootstrapOptions::in_memory())
        .expect("boot")
        .kernel;
    let root = k.root().expect("root");
    let mut g = Map::new();
    g.insert("guid".into(), json!("g1"));
    k.update(&root.id, g);
    for aid in ["alice", "bob", "eve", "maker"] {
        k.register(Agent::new(
            AgentId::from(aid),
            None,
            Some(root.id.clone()),
            json!({"can_call": {"creds": "*"}})
                .as_object()
                .cloned()
                .unwrap(),
            std::path::PathBuf::new(),
            false,
        ));
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let mut create = json!({
        "type": "create_agent", "parent_id": root.id.0,
        "handler_module": HANDLER_MODULE, "id": "creds",
        "callable_by": {"alice": "*", "bob": "*", "eve": "*", "maker": "*"},
        "file": tmp.path().join("creds.yaml").to_string_lossy(),
    });
    for (key, v) in meta.as_object().cloned().unwrap_or_default() {
        create[key] = v;
    }
    let rec = k
        .send(vec![Item::new(AgentId::from("kernel"), create)])
        .await
        .remove(0)
        .result;
    assert_eq!(rec["id"], json!("creds"), "{rec}");
    Fixture { k, keychain, tmp }
}

/// python `_call`.
async fn call(k: &Arc<Kernel>, payload: Value, as_: Option<&str>, secret: Option<&str>) -> Outcome {
    let mut item = Item::new(AgentId::from("creds"), payload);
    if let Some(s) = secret {
        item = item.with_secret(json!([s]));
    }
    match as_ {
        None => k.send(vec![item]).await.remove(0),
        Some(who) => with_sender(AgentId::from(who), k.send(vec![item]))
            .await
            .remove(0),
    }
}

const MARK_SET: Option<&str> = Some("payload.fields");
const MARK_GET: Option<&str> = Some("result");

fn store_meta(k: &Kernel, key: &str) -> Value {
    record(k, &AgentId::from("creds"))
        .get(key)
        .cloned()
        .unwrap_or(Value::Null)
}

/// python `_seed`: the operator stores `openai/main`, then names alice its reader
/// and bob its writer.
async fn seed(k: &Arc<Kernel>) -> Outcome {
    let out = call(
        k,
        json!({"type": "set", "ref": "openai/main", "host": "https://api.openai.com/v1",
               "fields": {"api_key": KEY}}),
        None,
        MARK_SET,
    )
    .await;
    assert_eq!(out.status.as_str(), "completed", "{}", out.result);
    let mut refs = store_meta(k, "refs");
    refs["openai/main"]["readers"] = json!(["alice"]);
    refs["openai/main"]["writers"] = json!(["bob"]);
    let mut patch = Map::new();
    patch.insert("refs".into(), refs);
    k.update(&AgentId::from("creds"), patch);
    out
}

// ─── the file backend ────────────────────────────────────────────────────────

#[tokio::test]
async fn set_stores_the_value_0600_and_the_record_keeps_only_metadata() {
    let f = mk(json!({})).await;
    let out = seed(&f.k).await;
    assert_eq!(
        out.result,
        json!({"ref": "openai/main", "host": "api.openai.com", "fields": ["api_key"], "stored": true})
    );
    let meta = std::fs::metadata(f.file()).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert!(std::fs::read_to_string(f.file()).unwrap().contains(KEY));
    let rec = Value::Object(record(&f.k, &AgentId::from("creds")));
    assert!(!rec.to_string().contains(KEY));
    assert_eq!(rec["refs"]["openai/main"]["host"], json!("api.openai.com"));
    assert_eq!(rec["refs"]["openai/main"]["fields"], json!(["api_key"]));
}

/// THE FILE IS PYTHON'S, BYTE FOR BYTE — `yaml.safe_dump({"refs": refs},
/// sort_keys=True, default_flow_style=False)` (measured with the python
/// kernel, 2026-10-01), so either runtime reads the other's.
#[tokio::test]
async fn the_file_is_pythons_bytes() {
    let f = mk(json!({})).await;
    for (r, fields) in [
        ("openai/main", json!({"api_key": KEY})),
        ("b", json!({"token": "x y", "api_key": "k-2"})),
    ] {
        let out = call(
            &f.k,
            json!({"type": "set", "ref": r, "host": "h.example", "fields": fields}),
            None,
            MARK_SET,
        )
        .await;
        assert_eq!(out.result["stored"], json!(true), "{}", out.result);
    }
    assert_eq!(
        std::fs::read_to_string(f.file()).unwrap(),
        "refs:\n  b:\n    api_key: k-2\n    token: x y\n  openai/main:\n    api_key: \
         sk-live-0123456789abcdef\n"
    );
    // and a python-written file reads back
    std::fs::write(
        f.file(),
        "refs:\n  openai/main:\n    api_key: sk-from-python\n",
    )
    .unwrap();
    std::fs::set_permissions(f.file(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let got = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        None,
        MARK_GET,
    )
    .await;
    assert_eq!(got.result["fields"], json!({"api_key": "sk-from-python"}));
}

#[tokio::test]
async fn values_move_only_with_a_secret_mark() {
    let f = mk(json!({})).await;
    let unmarked = call(
        &f.k,
        json!({"type": "set", "ref": "r", "host": "h", "fields": {"api_key": KEY}}),
        None,
        None,
    )
    .await;
    assert_eq!(unmarked.result["reason"], json!("bad_arg"));
    assert_eq!(unmarked.result["arg"], json!("secret"));
    assert_eq!(
        unmarked.result["error"],
        json!(
            "credential_store set: mark the values secret — secret: ['payload.fields'] — or \
             they would be recorded in the ledger, the memory store and the state stream"
        )
    );
    assert!(!f.file().exists(), "an unmarked set stores nothing");
    seed(&f.k).await;
    let unmarked_get = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        None,
        None,
    )
    .await;
    assert_eq!(
        unmarked_get.result["arg"],
        json!("secret"),
        "{}",
        unmarked_get.result
    );
    assert_eq!(
        unmarked_get.result["error"],
        json!(
            "credential_store get: mark the answer secret — secret: ['result'] — or the value \
             would be recorded in the ledger, the memory store and the state stream"
        )
    );
    let got = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        None,
        MARK_GET,
    )
    .await;
    assert_eq!(
        got.result,
        json!({"ref": "openai/main", "host": "api.openai.com", "fields": {"api_key": KEY}})
    );
    // `result.fields.<name>` for every field is a mark too
    let per_field =
        f.k.send(vec![Item::new(
            AgentId::from("creds"),
            json!({"type": "get", "ref": "openai/main"}),
        )
        .with_secret(json!(["result.fields.api_key"]))])
            .await
            .remove(0);
    assert_eq!(per_field.result["fields"], json!({"api_key": KEY}));
    let ledger =
        f.k.send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "ledger"}),
        )])
        .await
        .remove(0);
    assert!(
        !ledger.result.to_string().contains(KEY),
        "the ledger must never hold the value"
    );
}

#[tokio::test]
async fn readers_read_writers_rotate_strangers_see_nothing() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    // a reader reads
    let a = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        Some("alice"),
        MARK_GET,
    )
    .await;
    assert_eq!(a.result["fields"], json!({"api_key": KEY}), "{}", a.result);
    // a reader may not write — told so, since it knows the ref
    let w = call(
        &f.k,
        json!({"type": "set", "ref": "openai/main", "host": "api.openai.com",
               "fields": {"api_key": KEY2}}),
        Some("alice"),
        MARK_SET,
    )
    .await;
    assert_eq!(w.result["reason"], json!("unauthorized"), "{}", w.result);
    assert_eq!(
        w.result["error"],
        json!("credential_store: alice may read 'openai/main' but not write it")
    );
    // a writer rotates; the reader's next read gets the new value
    let r = call(
        &f.k,
        json!({"type": "set", "ref": "openai/main", "host": "api.openai.com",
               "fields": {"api_key": KEY2}}),
        Some("bob"),
        MARK_SET,
    )
    .await;
    assert_eq!(r.result["stored"], json!(true), "{}", r.result);
    let a2 = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        Some("alice"),
        MARK_GET,
    )
    .await;
    assert_eq!(a2.result["fields"], json!({"api_key": KEY2}));
    // a stranger is answered as if the ref did not exist
    let e = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        Some("eve"),
        MARK_GET,
    )
    .await;
    assert_eq!(
        e.result,
        json!({"error": "credential_store: no such ref 'openai/main'", "reason": "not_found",
               "arg": "ref"})
    );
    assert_eq!(
        call(&f.k, json!({"type": "list"}), Some("eve"), None)
            .await
            .result,
        json!({"refs": []})
    );
    // list is metadata only
    let listed = call(&f.k, json!({"type": "list"}), Some("alice"), None)
        .await
        .result;
    assert_eq!(
        listed["refs"],
        json!([{"ref": "openai/main", "host": "api.openai.com", "fields": ["api_key"],
                "read": true, "write": false}])
    );
    assert!(!listed.to_string().contains(KEY2));
    // the operator's listing carries the grants
    let op = call(&f.k, json!({"type": "list"}), None, None).await.result;
    assert_eq!(
        op["refs"],
        json!([{"ref": "openai/main", "host": "api.openai.com", "fields": ["api_key"],
                "read": true, "write": true, "readers": ["alice"], "writers": ["bob"]}])
    );
}

#[tokio::test]
async fn a_creator_owns_the_ref_it_makes_others_cannot_make_one() {
    let f = mk(json!({"creators": ["maker"]})).await;
    let made = call(
        &f.k,
        json!({"type": "set", "ref": "mine", "host": "x.example", "fields": {"token": KEY}}),
        Some("maker"),
        MARK_SET,
    )
    .await;
    assert_eq!(made.result["stored"], json!(true), "{}", made.result);
    let entry = &store_meta(&f.k, "refs")["mine"];
    assert_eq!(entry["readers"], json!(["maker"]));
    assert_eq!(entry["writers"], json!(["maker"]));
    // the record's key order: the grants, then host and the field NAMES
    assert_eq!(
        entry.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["readers", "writers", "host", "fields"]
    );
    let nope = call(
        &f.k,
        json!({"type": "set", "ref": "theirs", "host": "x.example", "fields": {"token": KEY}}),
        Some("eve"),
        MARK_SET,
    )
    .await;
    assert_eq!(nope.result["reason"], json!("not_found"), "{}", nope.result);
}

#[tokio::test]
async fn a_subtree_grant_covers_descendants() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    f.k.register(Agent::new(
        AgentId::from("child"),
        None,
        Some(AgentId::from("alice")),
        json!({"can_call": {"creds": "*"}})
            .as_object()
            .cloned()
            .unwrap(),
        std::path::PathBuf::new(),
        false,
    ));
    let mut cb = store_meta(&f.k, "callable_by");
    cb["child"] = json!("*");
    let mut refs = store_meta(&f.k, "refs");
    refs["openai/main"]["readers"] = json!(["$subtree:alice"]);
    let mut patch = Map::new();
    patch.insert("refs".into(), refs);
    patch.insert("callable_by".into(), cb);
    f.k.update(&AgentId::from("creds"), patch);
    let got = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        Some("child"),
        MARK_GET,
    )
    .await;
    assert_eq!(
        got.result["fields"],
        json!({"api_key": KEY}),
        "{}",
        got.result
    );
    // a sibling outside the subtree is not covered
    let e = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        Some("eve"),
        MARK_GET,
    )
    .await;
    assert_eq!(e.result["reason"], json!("not_found"));
}

#[tokio::test]
async fn forget_drops_the_value_and_the_grants() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    // a reader is no writer: the ref does not exist for it here
    let a = call(
        &f.k,
        json!({"type": "forget", "ref": "openai/main"}),
        Some("alice"),
        None,
    )
    .await;
    assert_eq!(a.result["reason"], json!("not_found"));
    let out = call(
        &f.k,
        json!({"type": "forget", "ref": "openai/main"}),
        Some("bob"),
        None,
    )
    .await;
    assert_eq!(out.result, json!({"forgotten": true, "ref": "openai/main"}));
    assert!(store_meta(&f.k, "refs").get("openai/main").is_none());
    assert!(!std::fs::read_to_string(f.file()).unwrap().contains(KEY));
}

#[tokio::test]
async fn a_file_others_can_read_is_refused_loud() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    std::fs::set_permissions(f.file(), std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        None,
        MARK_GET,
    )
    .await;
    assert_eq!(out.status.as_str(), "failed", "{}", out.result);
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    assert_eq!(
        out.result["error"],
        json!(format!(
            "PermissionError: credential_store: credential file {} must be a regular file owned \
             by this user with mode 0600 (is 0o644, uid {uid}) — refused, nothing read",
            f.file().display()
        ))
    );
    assert_eq!(out.result["raised"], json!(true));
    // a symlink is not a regular file — refused before it is followed
    std::fs::set_permissions(f.file(), std::fs::Permissions::from_mode(0o600)).unwrap();
    let real = f.tmp.path().join("real.yaml");
    std::fs::rename(f.file(), &real).unwrap();
    std::os::unix::fs::symlink(&real, f.file()).unwrap();
    let out = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        None,
        MARK_GET,
    )
    .await;
    assert_eq!(out.status.as_str(), "failed", "{}", out.result);
    assert!(out.result["error"]
        .as_str()
        .unwrap()
        .contains("must be a regular file"));
}

/// A RELATIVE `file` lives under the kernel's running dir, clamped there.
#[tokio::test]
async fn a_relative_file_is_clamped_under_the_running_dir() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(HANDLER_MODULE, crate::test_support::bundle());
    let k = bootstrap(reg, BootstrapOptions::one_shot(tmp.path()))
        .expect("boot")
        .kernel;
    let root = k.root().unwrap().id.clone();
    for (id, file) in [("inside", "sub/creds.yaml"), ("outside", "../creds.yaml")] {
        let r = k
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type": "create_agent", "parent_id": root.0, "id": id,
                       "handler_module": HANDLER_MODULE, "file": file}),
            )])
            .await
            .remove(0)
            .result;
        assert_eq!(r["id"], json!(id), "{r}");
    }
    let set = |id: &str| {
        Item::new(
            AgentId::from(id),
            json!({"type": "set", "ref": "r", "host": "h", "fields": {"k": "v"}}),
        )
        .with_secret(json!(["payload.fields"]))
    };
    let ok = k.send(vec![set("inside")]).await.remove(0);
    assert_eq!(ok.result["stored"], json!(true), "{}", ok.result);
    assert!(tmp.path().join("sub/creds.yaml").is_file());
    let refused = k.send(vec![set("outside")]).await.remove(0);
    assert_eq!(refused.status.as_str(), "failed");
    assert_eq!(
        refused.result["error"],
        json!("ValueError: fs: root '..' escapes the running dir")
    );
}

// ─── the keychain backend (an in-memory keychain — never the real one) ───────

#[tokio::test]
async fn the_keychain_backend_keeps_one_item_per_ref() {
    let f = mk(json!({"backend": "keychain"})).await;
    seed(&f.k).await;
    assert_eq!(
        f.keychain.accounts(),
        [("fantastic".to_string(), "creds/openai/main".to_string())]
    );
    assert_eq!(
        f.keychain
            .get_password("fantastic", "creds/openai/main")
            .unwrap(),
        Some(format!("{{\"api_key\": \"{KEY}\"}}"))
    );
    assert!(!f.file().exists(), "no file is written");
    let got = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        None,
        MARK_GET,
    )
    .await;
    assert_eq!(got.result["fields"], json!({"api_key": KEY}));
    call(
        &f.k,
        json!({"type": "forget", "ref": "openai/main"}),
        None,
        None,
    )
    .await;
    assert!(f.keychain.accounts().is_empty());
    // the reflect names the backend and no file
    let r = call(&f.k, json!({"type": "reflect"}), None, None)
        .await
        .result;
    assert_eq!(r["backend"], json!("keychain"));
    assert!(r.get("file").is_none());
}

/// A platform with no keychain in this runtime fails LOUD, naming it — never
/// another backend.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
#[test]
fn no_os_keychain_is_a_loud_failure_naming_the_platform() {
    let err = OsKeychain.get_password("fantastic", "x/y").unwrap_err();
    assert!(err.contains(std::env::consts::OS), "{err}");
}

// ─── the env backend (read-only) ─────────────────────────────────────────────

#[tokio::test]
async fn the_env_backend_reads_named_variables_and_refuses_writes() {
    // a variable no other test names (the environment is process-wide)
    let var = "CRED_STORE_TEST_KEY_ENV_BACKEND";
    let f = mk(json!({
        "backend": "env",
        "refs": {"ci/key": {"readers": ["alice"], "host": "api.example",
                            "env": {"api_key": var}}},
    }))
    .await;
    std::env::set_var(var, KEY);
    let got = call(
        &f.k,
        json!({"type": "get", "ref": "ci/key"}),
        Some("alice"),
        MARK_GET,
    )
    .await;
    assert_eq!(
        got.result,
        json!({"ref": "ci/key", "host": "api.example", "fields": {"api_key": KEY}})
    );
    let w = call(
        &f.k,
        json!({"type": "set", "ref": "ci/key", "host": "api.example",
               "fields": {"api_key": KEY2}}),
        None,
        MARK_SET,
    )
    .await;
    assert_eq!(w.result["arg"], json!("backend"), "{}", w.result);
    std::env::remove_var(var);
    let gone = call(
        &f.k,
        json!({"type": "get", "ref": "ci/key"}),
        Some("alice"),
        MARK_GET,
    )
    .await;
    assert_eq!(gone.status.as_str(), "failed", "{}", gone.result);
    assert_eq!(
        gone.result["error"],
        json!(format!(
            "LookupError: credential_store: ref 'ci/key' reads 'api_key' from ${var}, which is \
             unset"
        ))
    );
    // its listing names the fields from the env map
    let listed = call(&f.k, json!({"type": "list"}), Some("alice"), None)
        .await
        .result;
    assert_eq!(listed["refs"][0]["fields"], json!(["api_key"]));
}

// ─── the host a ref is bound to ──────────────────────────────────────────────

#[tokio::test]
async fn a_read_pinned_to_another_host_is_refused() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    let bad = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main", "host": "https://evil.example.net/v1"}),
        None,
        MARK_GET,
    )
    .await;
    assert_eq!(
        bad.result,
        json!({"error": "credential_store: ref 'openai/main' is bound to host \
                         'api.openai.com', not 'evil.example.net' — refused, nothing read",
               "reason": "bad_arg", "arg": "host"})
    );
    let good = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main",
               "host": "HTTPS://API.OpenAI.com/v1/responses"}),
        None,
        MARK_GET,
    )
    .await;
    assert_eq!(
        good.result["fields"],
        json!({"api_key": KEY}),
        "{}",
        good.result
    );
    // a host that names nothing is no host
    let blank = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main", "host": "  "}),
        None,
        MARK_GET,
    )
    .await;
    assert!(blank.result["error"]
        .as_str()
        .unwrap()
        .ends_with("not None — refused, nothing read"));
}

#[test]
fn host_is_pythons_urlsplit_netloc_lowercased() {
    let h = |s: &str| host(Some(&json!(s)));
    assert_eq!(
        h("https://api.openai.com/v1").as_deref(),
        Some("api.openai.com")
    );
    assert_eq!(
        h("http://127.0.0.1:8080/x?y").as_deref(),
        Some("127.0.0.1:8080")
    );
    assert_eq!(h("  API.Example.COM  ").as_deref(), Some("api.example.com"));
    assert_eq!(h("://nohost"), None);
    assert_eq!(h("1http://x/y"), None);
    assert_eq!(h(""), None);
    assert_eq!(host(Some(&json!(5))), None);
    assert_eq!(host(None), None);
}

// ─── the record ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_bad_record_is_refused_at_the_write() {
    let f = mk(json!({})).await;
    let root = f.k.root().unwrap().id.clone();
    let r =
        f.k.send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "create_agent", "parent_id": root.0,
                   "handler_module": HANDLER_MODULE, "id": "bad", "backend": "vault"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(r["reason"], json!("invalid_meta"), "{r}");
    assert_eq!(
        r["error"],
        json!("backend='vault' is not one of file, keychain, env")
    );
}

const HOLDS: &str = "holds_secrets must be true — a credential_store always holds secrets";

/// The KERNEL's shape refusal — judged before the store's `validate_meta`.
fn shape(got: &str) -> String {
    format!(
        "holds_secrets must be exactly true (this agent holds secrets) or null \
         (the operator clears the mark) — got {got}"
    )
}

/// python `test_a_store_is_a_secret_holder_and_cannot_stop_being_one`: the store
/// stamps `holds_secrets: true` on its own record at boot (ai_core refuses every
/// model-parsed call to an agent so marked), shows it on reflect, and refuses any
/// other value at the write — create and update alike.
///
/// Two checks judge the value, in a FIXED order: the kernel's shape check first
/// (anything but `true`/`null` is malformed on every agent — its text), then the
/// store's `validate_meta` (it refuses `null`, the operator's clear, on itself —
/// the store's text). So `null` gets HOLDS and every other value the kernel's.
#[tokio::test]
async fn a_store_is_a_secret_holder_and_cannot_stop_being_one() {
    let f = mk(json!({})).await;
    let op = |payload: Value| {
        let k = Arc::clone(&f.k);
        async move {
            k.send(vec![Item::new(AgentId::from("kernel"), payload)])
                .await
                .remove(0)
                .result
        }
    };
    assert_eq!(store_meta(&f.k, HOLDS_SECRETS), json!(true));
    let shown = call(&f.k, json!({"type": "reflect"}), None, None).await;
    assert_eq!(
        shown.result["holds_secrets"],
        json!(true),
        "{}",
        shown.result
    );
    for (bad, why) in [
        (Value::Null, HOLDS.to_string()),
        (json!(false), shape("bool")),
        (json!("true"), shape("str")),
        (json!(1), shape("int")),
    ] {
        let r = op(json!({"type": "update_agent", "id": "creds", "holds_secrets": bad})).await;
        assert_eq!(
            r,
            json!({"error": why, "reason": "invalid_meta", "id": "creds"}),
            "{bad}"
        );
    }
    assert_eq!(store_meta(&f.k, HOLDS_SECRETS), json!(true));
    let ok = op(json!({"type": "update_agent", "id": "creds", "holds_secrets": true})).await;
    assert_eq!(ok["updated"], json!(true), "{ok}");
    let root = f.k.root().unwrap().id.clone();
    for (bad, why) in [
        (Value::Null, HOLDS.to_string()),
        (json!(false), shape("bool")),
    ] {
        let r = op(json!({"type": "create_agent", "parent_id": root.0,
                          "handler_module": HANDLER_MODULE, "id": "creds2",
                          "holds_secrets": bad}))
        .await;
        assert_eq!(r, json!({"error": why, "reason": "invalid_meta"}), "{bad}");
        assert!(!f.k.agents.contains_key(&AgentId::from("creds2")));
    }
}

#[test]
fn validate_meta_texts_are_pythons() {
    let b = CredentialStoreBundle::default();
    let v = |m: Value| b.validate_meta(m.as_object().unwrap(), &Map::new());
    assert_eq!(v(json!({"backend": null})), None);
    assert_eq!(v(json!({"holds_secrets": true})), None);
    for bad in [json!(false), Value::Null, json!("true"), json!(1)] {
        assert_eq!(v(json!({"holds_secrets": bad})).as_deref(), Some(HOLDS));
    }
    assert_eq!(
        v(json!({"file": " "})).as_deref(),
        Some("file must be a non-empty string")
    );
    assert_eq!(
        v(json!({"keychain_service": 3})).as_deref(),
        Some("keychain_service must be a non-empty string")
    );
    assert_eq!(
        v(json!({"creators": ["a", ""]})).as_deref(),
        Some("creators must be a list of agent ids")
    );
    assert_eq!(
        v(json!({"refs": []})).as_deref(),
        Some("refs must be a map of ref to {readers, writers, host, fields, env}")
    );
    assert_eq!(
        v(json!({"refs": {" ": {}}})).as_deref(),
        Some("a ref must be a non-empty string")
    );
    assert_eq!(
        v(json!({"refs": {"r": 1}})).as_deref(),
        Some("refs['r'] must be a map")
    );
    assert_eq!(
        v(json!({"refs": {"r": {"readers": "a"}}})).as_deref(),
        Some("refs['r'].readers must be a list of strings")
    );
    assert_eq!(
        v(json!({"refs": {"r": {"host": "://"}}})).as_deref(),
        Some("refs['r'].host must be a non-empty host")
    );
    assert_eq!(
        v(json!({"refs": {"r": {"env": {"api_key": ""}}}})).as_deref(),
        Some("refs['r'].env must map field names to variable names")
    );
    assert_eq!(
        v(
            json!({"refs": {"r": {"readers": ["a"], "host": "h", "fields": ["k"],
                                "env": {"k": "V"}}}})
        ),
        None
    );
}

#[tokio::test]
async fn reflect_is_posture_never_a_value_or_a_ref_name() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    let r = call(&f.k, json!({"type": "reflect"}), None, None)
        .await
        .result;
    let keys: Vec<&String> = r.as_object().unwrap().keys().take(6).collect();
    assert_eq!(keys, ["id", "sentence", "backend", "refs", "verbs", "file"]);
    assert_eq!(r["backend"], json!("file"));
    assert_eq!(r["refs"], json!(1));
    assert_eq!(r["file"], json!(f.file().to_string_lossy()));
    assert_eq!(
        r["verbs"].as_object().unwrap().keys().collect::<Vec<_>>(),
        ["reflect", "boot", "set", "get", "list", "forget"]
    );
    let text = r.to_string();
    assert!(!text.contains(KEY) && !text.contains("openai/main"));
    // with no `file`, the default under the user's home is NAMED, never touched
    assert_eq!(
        CredentialStoreBundle::reflect(&AgentId::from("c2"), &Map::new())["file"],
        json!("~/.fantastic/credentials/c2.yaml")
    );
}

#[tokio::test]
async fn missing_args_and_unknown_verbs_are_refused_in_pythons_words() {
    let f = mk(json!({})).await;
    for (payload, err, arg) in [
        (
            json!({"type": "set", "host": "h", "fields": {"k": "v"}}),
            "credential_store set: ref required",
            "ref",
        ),
        (
            json!({"type": "set", "ref": "r", "fields": {"k": "v"}}),
            "credential_store set: host required",
            "host",
        ),
        (
            json!({"type": "get", "ref": " "}),
            "credential_store get: ref required",
            "ref",
        ),
        (
            json!({"type": "forget"}),
            "credential_store forget: ref required",
            "ref",
        ),
    ] {
        let out = call(&f.k, payload, None, None).await.result;
        assert_eq!(
            out,
            json!({"error": err, "reason": "missing_arg", "arg": arg})
        );
    }
    let bad = call(
        &f.k,
        json!({"type": "set", "ref": "r", "host": "h", "fields": {"k": 1}}),
        None,
        None,
    )
    .await
    .result;
    assert_eq!(
        bad,
        json!({"error": "credential_store set: fields must be a non-empty map of field name to \
                         string value", "reason": "bad_arg", "arg": "fields"})
    );
    let unk = call(&f.k, json!({"type": "shutdown"}), None, None)
        .await
        .result;
    assert_eq!(
        unk,
        json!({"error": "credential_store: unknown type 'shutdown'", "reason": "unhandled",
               "verb": "shutdown"})
    );
}

/// Concurrent writes lose nothing: python's read-modify-write runs with no await
/// in between; here dispatches run on many threads, so the store serializes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sets_lose_no_ref() {
    let f = mk(json!({})).await;
    let items: Vec<Item> = (0..16)
        .map(|i| {
            Item::new(
                AgentId::from("creds"),
                json!({"type": "set", "ref": format!("r{i:02}"), "host": "h.example",
                       "fields": {"api_key": format!("k{i}")}}),
            )
            .with_secret(json!(["payload.fields"]))
        })
        .collect();
    for out in f.k.send(items).await {
        assert_eq!(out.result["stored"], json!(true), "{}", out.result);
    }
    assert_eq!(store_meta(&f.k, "refs").as_object().unwrap().len(), 16);
    let text = std::fs::read_to_string(f.file()).unwrap();
    assert!(
        (0..16).all(|i| text.contains(&format!("r{i:02}:"))),
        "{text}"
    );
}
