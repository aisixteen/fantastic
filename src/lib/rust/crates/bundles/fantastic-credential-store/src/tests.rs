//! credential_store: secrets by ref, bound to a host, read by whoever the call
//! gate admits — the twin of python
//! `credential_store/tests/test_credential_store.py`. Tests never touch the real
//! keychain or home: the file backend writes under a temp dir, the keychain
//! backend runs on an in-memory keychain.

use super::*;
use crate::keychain::MemoryKeychain;
use fantastic_file::test_support::{mode_of, own_uid, set_mode};
use fantastic_kernel::agent::Agent;
use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::bundle::BundleRegistry;
use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::test_support::register_root_loader;
use fantastic_kernel::test_support::symlink;
use fantastic_kernel::{Item, Outcome};
use std::path::Path;

const KEY: &str = "sk-live-0123456789abcdef";
const KEY2: &str = "sk-live-rotated-fedcba98";

#[test]
fn field_marks_names_each_field_or_the_whole_map() {
    assert_eq!(
        field_marks(Some(&json!({"api_key": "x", "user": "y"}))),
        vec!["fields.api_key".to_string(), "fields.user".to_string()]
    );
    assert_eq!(
        field_marks(Some(&json!({"a.b": "x"}))),
        vec!["fields".to_string()]
    );
    assert_eq!(field_marks(Some(&json!("x"))), vec!["fields".to_string()]);
    assert!(field_marks(None).is_empty());
}

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
async fn call(k: &Arc<Kernel>, payload: Value, as_: Option<&str>) -> Outcome {
    let item = Item::new(AgentId::from("creds"), payload);
    match as_ {
        None => k.send(vec![item]).await.remove(0),
        Some(who) => as_agent(AgentId::from(who), k.send(vec![item]))
            .await
            .remove(0),
    }
}

fn store_meta(k: &Kernel, key: &str) -> Value {
    record(k, &AgentId::from("creds"))
        .get(key)
        .cloned()
        .unwrap_or(Value::Null)
}

/// The kernel's own answer to an operator call.
async fn op(k: &Arc<Kernel>, payload: Value) -> Value {
    k.send(vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
        .result
}

/// python `_declare`: declare refs the ONE way there is — the operator's
/// `update_agent` on the store's `refs`. It replaces the map, so the refs already
/// there ride along.
async fn declare(k: &Arc<Kernel>, refs: Value) {
    let mut all = store_meta(k, "refs")
        .as_object()
        .cloned()
        .unwrap_or_default();
    for (r, e) in refs.as_object().cloned().unwrap_or_default() {
        all.insert(r, e);
    }
    let out = op(
        k,
        json!({"type": "update_agent", "id": "creds", "refs": Value::Object(all)}),
    )
    .await;
    assert_eq!(out["updated"], json!(true), "{out}");
}

/// python `_seed`: the operator declares `openai/main` — alice its reader, bob its
/// writer — then stores its value.
async fn seed(k: &Arc<Kernel>) -> Outcome {
    declare(k, json!({"openai/main": {"host": "api.openai.com"}})).await;
    let out = call(
        k,
        json!({"type": "set", "ref": "openai/main", "fields": {"api_key": KEY}}),
        None,
    )
    .await;
    assert_eq!(out.status.as_str(), "completed", "{}", out.result);
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
    assert_eq!(mode_of(f.file()).unwrap() & 0o777, 0o600);
    assert!(std::fs::read_to_string(f.file()).unwrap().contains(KEY));
    let rec = Value::Object(record(&f.k, &AgentId::from("creds")));
    assert!(!rec.to_string().contains(KEY));
    assert_eq!(rec["refs"]["openai/main"]["host"], json!("api.openai.com"));
    assert_eq!(rec["refs"]["openai/main"]["fields"], json!(["api_key"]));
}

/// THE FILE IS PYTHON'S, BYTE FOR BYTE — the kernel's YAML emitter over every
/// map's keys sorted, each ref's entry `{fields, host}` (the host stored WITH the
/// value), so either runtime reads the other's.
#[tokio::test]
async fn the_file_is_pythons_bytes() {
    let f = mk(json!({})).await;
    declare(
        &f.k,
        json!({"openai/main": {"host": "h.example"}, "b": {"host": "h.example"}}),
    )
    .await;
    for (r, fields) in [
        ("openai/main", json!({"api_key": KEY})),
        ("b", json!({"token": "x y", "api_key": "k-2"})),
    ] {
        let out = call(
            &f.k,
            json!({"type": "set", "ref": r, "fields": fields}),
            None,
        )
        .await;
        assert_eq!(out.result["stored"], json!(true), "{}", out.result);
    }
    assert_eq!(
        std::fs::read_to_string(f.file()).unwrap(),
        "refs:\n  b:\n    fields:\n      api_key: k-2\n      token: x y\n    host: \
         h.example\n  openai/main:\n    fields:\n      api_key: sk-live-0123456789abcdef\n    \
         host: h.example\n"
    );
    // and a python-written file reads back
    std::fs::write(
        f.file(),
        "refs:\n  openai/main:\n    fields:\n      api_key: sk-from-python\n    host: \
         h.example\n",
    )
    .unwrap();
    set_mode(f.file(), 0o600).unwrap();
    let got = call(&f.k, json!({"type": "get", "ref": "openai/main"}), None).await;
    assert_eq!(got.result["fields"], json!({"api_key": "sk-from-python"}));
}

/// A ref is declared ONLY by `update_agent` on the store's `refs`. `set` on an
/// undeclared ref answers "no such ref" — to an agent AND to the operator — and
/// stores nothing; once declared, its writer stores and its reader reads.
#[tokio::test]
async fn set_never_creates_a_ref_update_agent_declares_it() {
    let f = mk(json!({})).await;
    let put = json!({"type": "set", "ref": "mine", "fields": {"token": KEY}});
    for who in [None, Some("maker")] {
        let nope = call(&f.k, put.clone(), who).await;
        assert_eq!(
            nope.result,
            json!({"error": "credential_store: no such ref 'mine'", "reason": "not_found",
                   "arg": "ref"}),
            "{who:?}"
        );
    }
    assert!(store_meta(&f.k, "refs").get("mine").is_none());
    assert!(!f.file().exists(), "an undeclared set stores nothing");
    declare(&f.k, json!({"mine": {"host": "x.example"}})).await;
    let made = call(&f.k, put.clone(), Some("maker")).await;
    assert_eq!(made.result["stored"], json!(true), "{}", made.result);
    let got = call(&f.k, json!({"type": "get", "ref": "mine"}), Some("maker")).await;
    assert_eq!(
        got.result["fields"],
        json!({"token": KEY}),
        "{}",
        got.result
    );
    // the record's key order: the declaration's, then the field NAMES
    let entry = &store_meta(&f.k, "refs")["mine"];
    assert_eq!(
        entry.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["host", "fields"]
    );
    let mut theirs = put;
    theirs["ref"] = json!("theirs");
    let nope = call(&f.k, theirs, Some("eve")).await;
    assert_eq!(nope.result["reason"], json!("not_found"), "{}", nope.result);
}

/// `forget` drops the VALUE — from the backend, with
/// its field names — and nothing else. The declaration (its host) is the operator's, written and removed by `update_agent` alone, so a writer can
/// never undeclare a ref nobody could then set again.
#[tokio::test]
async fn forget_drops_the_value_and_keeps_the_declaration() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    let declared = store_meta(&f.k, "refs")["openai/main"].clone();
    let out = call(
        &f.k,
        json!({"type": "forget", "ref": "openai/main"}),
        Some("bob"),
    )
    .await;
    assert_eq!(out.result, json!({"forgotten": true, "ref": "openai/main"}));
    assert!(!std::fs::read_to_string(f.file()).unwrap().contains(KEY));
    assert_eq!(
        store_meta(&f.k, "refs")["openai/main"],
        json!({"host": declared["host"]})
    );
    // a reader now finds the ref declared and empty
    let empty = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        Some("alice"),
    )
    .await;
    assert_eq!(
        empty.result,
        json!({"error": "credential_store: ref 'openai/main' has no value in the backend",
               "reason": "not_found", "arg": "ref"})
    );
    // its writer sets it again, and the reader reads the new value
    let back = call(
        &f.k,
        json!({"type": "set", "ref": "openai/main", "fields": {"api_key": KEY2}}),
        Some("bob"),
    )
    .await;
    assert_eq!(back.result["stored"], json!(true), "{}", back.result);
    let got = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        Some("alice"),
    )
    .await;
    assert_eq!(
        got.result["fields"],
        json!({"api_key": KEY2}),
        "{}",
        got.result
    );
    // forget again drops the value; the declaration stays — undeclaring is
    // update_agent's alone
    let again = call(
        &f.k,
        json!({"type": "forget", "ref": "openai/main"}),
        Some("eve"),
    )
    .await;
    assert_eq!(
        again.result,
        json!({"forgotten": true, "ref": "openai/main"})
    );
    assert_eq!(
        store_meta(&f.k, "refs")["openai/main"],
        json!({"host": declared["host"]})
    );
}

/// A ref's host is its DECLARATION — written by
/// `update_agent` on the store's `refs`, the one path. `set` stores the value of
/// a declared ref and takes no host: a writer naming one is refused, loud, with
/// nothing stored and the declared host untouched, so a writer can never rebind a
/// ref (and deny every reader at the real host).
#[tokio::test]
async fn set_takes_no_host_the_declaration_is_the_binding() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    let rebind = call(
        &f.k,
        json!({"type": "set", "ref": "openai/main", "host": "attacker.example",
               "fields": {"api_key": KEY2}}),
        Some("bob"),
    )
    .await;
    assert_eq!(
        rebind.result,
        json!({"error": "credential_store set: takes no host — a ref's host is its \
                         declaration, written by update_agent on this store's refs",
               "reason": "bad_arg", "arg": "host"})
    );
    assert_eq!(
        store_meta(&f.k, "refs")["openai/main"]["host"],
        json!("api.openai.com")
    );
    assert!(
        !std::fs::read_to_string(f.file()).unwrap().contains(KEY2),
        "nothing stored"
    );
    // the operator too: the host moves only with the declaration
    let op_set = call(
        &f.k,
        json!({"type": "set", "ref": "openai/main", "host": "api.openai.com",
               "fields": {"api_key": KEY2}}),
        None,
    )
    .await;
    assert_eq!(op_set.result["arg"], json!("host"), "{}", op_set.result);
    // a set with no host stores the value under the declared host
    let ok = call(
        &f.k,
        json!({"type": "set", "ref": "openai/main", "fields": {"api_key": KEY2}}),
        Some("bob"),
    )
    .await;
    assert_eq!(
        ok.result,
        json!({"ref": "openai/main", "host": "api.openai.com", "fields": ["api_key"],
               "stored": true})
    );
    let got = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main", "host": "https://api.openai.com/v1"}),
        Some("alice"),
    )
    .await;
    assert_eq!(
        got.result["fields"],
        json!({"api_key": KEY2}),
        "{}",
        got.result
    );
}

/// The host is the binding, so a declaration without one is refused at the
/// write: a value stored under it could be sent nowhere — or anywhere.
#[tokio::test]
async fn a_ref_is_declared_with_its_host() {
    let f = mk(json!({})).await;
    let r = op(
        &f.k,
        json!({"type": "update_agent", "id": "creds",
               "refs": {"nohost": {}}}),
    )
    .await;
    assert_eq!(
        r,
        json!({"error": "refs['nohost'].host is required — the one host this ref's value \
                         may be sent to",
               "reason": "invalid_meta", "id": "creds"})
    );
}

/// The env backend is read-only: its value is the variable, so `forget` — which
/// drops only a value — has nothing it may drop, and says so.
#[tokio::test]
async fn forget_on_the_env_backend_is_refused() {
    let f = mk(json!({
        "backend": "env",
        "refs": {"ci/key": {"host": "api.example", "env": {"api_key": "CI_TEST_KEY"}}},
    }))
    .await;
    let out = call(&f.k, json!({"type": "forget", "ref": "ci/key"}), None).await;
    assert_eq!(
        out.result,
        json!({"error": "credential_store forget: the env backend is read-only — unset the \
                         environment variable its ref names",
               "reason": "bad_arg", "arg": "backend"})
    );
    assert!(store_meta(&f.k, "refs").get("ci/key").is_some());
}

#[tokio::test]
async fn a_file_others_can_read_is_refused_loud() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    set_mode(f.file(), 0o644).unwrap();
    let out = call(&f.k, json!({"type": "get", "ref": "openai/main"}), None).await;
    assert_eq!(out.status.as_str(), "failed", "{}", out.result);
    let uid = own_uid();
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
    set_mode(f.file(), 0o600).unwrap();
    let real = f.tmp.path().join("real.yaml");
    std::fs::rename(f.file(), &real).unwrap();
    symlink(&real, f.file()).unwrap();
    let out = call(&f.k, json!({"type": "get", "ref": "openai/main"}), None).await;
    assert_eq!(out.status.as_str(), "failed", "{}", out.result);
    assert!(out.result["error"]
        .as_str()
        .unwrap()
        .contains("must be a regular file"));
}

fn relative(p: &str) -> String {
    format!(
        "file={} must be an absolute path (starting with / or ~) — a relative path is refused",
        py_repr(p)
    )
}

/// `file` is `/…` or `~…` — never relative to the kernel's running dir. A
/// relative one is refused AT THE WRITE (create and update alike), and a record
/// that carries one past the write (an in-process register) is refused at use,
/// loud, with nothing read or written.
#[tokio::test]
async fn the_file_is_an_absolute_path_a_relative_one_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut reg = BundleRegistry::new();
    register_root_loader(&mut reg);
    reg.register(HANDLER_MODULE, crate::test_support::bundle());
    let k = bootstrap(reg, BootstrapOptions::one_shot(tmp.path()))
        .expect("boot")
        .kernel;
    let root = k.root().unwrap().id.clone();
    for rel in ["creds.yaml", "sub/creds.yaml", "./creds.yaml"] {
        let r = op(
            &k,
            json!({"type": "create_agent", "parent_id": root.0, "id": "rel",
                   "handler_module": HANDLER_MODULE, "file": rel}),
        )
        .await;
        assert_eq!(
            r,
            json!({"error": relative(rel), "reason": "invalid_meta"}),
            "{rel}"
        );
        assert!(!k.agents.contains_key(&AgentId::from("rel")));
    }
    let abs = tmp.path().join("creds.yaml").to_string_lossy().to_string();
    let made = op(
        &k,
        json!({"type": "create_agent", "parent_id": root.0, "id": "creds",
               "handler_module": HANDLER_MODULE, "file": abs}),
    )
    .await;
    assert_eq!(made["id"], json!("creds"), "{made}");
    let r = op(
        &k,
        json!({"type": "update_agent", "id": "creds", "file": "creds.yaml"}),
    )
    .await;
    assert_eq!(
        r,
        json!({"error": relative("creds.yaml"), "reason": "invalid_meta", "id": "creds"})
    );
    assert_eq!(store_meta(&k, "file"), json!(abs));
    // `~…` passes the write (no disk is touched to judge it)
    let b = CredentialStoreBundle::default();
    let v = |m: Value| b.validate_meta(m.as_object().unwrap(), &Map::new());
    assert_eq!(
        v(json!({"file": "~/.fantastic/user/credentials/x.yaml"})),
        None
    );
    assert_eq!(
        v(json!({"keychain_namespace": ""})).as_deref(),
        Some("keychain_namespace must be a non-empty string")
    );
    // past the write: refused at use, nothing written under the running dir
    k.register(Agent::new(
        AgentId::from("sneaky"),
        Some(HANDLER_MODULE.to_string()),
        Some(root.clone()),
        json!({"file": "creds.yaml", "refs": {"r": {"host": "h"}}})
            .as_object()
            .cloned()
            .unwrap(),
        std::path::PathBuf::new(),
        false,
    ));
    let out = k
        .send(vec![Item::new(
            AgentId::from("sneaky"),
            json!({"type": "set", "ref": "r", "fields": {"api_key": KEY}}),
        )])
        .await
        .remove(0);
    assert_eq!(out.status.as_str(), "failed", "{}", out.result);
    assert_eq!(
        out.result["error"],
        json!(format!(
            "ValueError: credential_store: {}",
            relative("creds.yaml")
        ))
    );
    assert!(!k.workdir().unwrap().join("creds.yaml").exists());
    assert!(!tmp.path().join("creds.yaml").exists());
}

// ─── the keychain backend (an in-memory keychain — never the real one) ───────

/// One item per ref, under `keychain_namespace` (default `fantastic` — the
/// keychain's own word for it is the item's "service"), account `<id>/<ref>`.
#[tokio::test]
async fn the_keychain_backend_keeps_one_item_per_ref() {
    for (meta, namespace) in [
        (json!({"backend": "keychain"}), "fantastic"),
        (
            json!({"backend": "keychain", "keychain_namespace": "acme"}),
            "acme",
        ),
    ] {
        let f = mk(meta).await;
        seed(&f.k).await;
        assert_eq!(
            f.keychain.accounts(),
            [(namespace.to_string(), "creds/openai/main".to_string())]
        );
        assert_eq!(
            f.keychain
                .get_password(namespace, "creds/openai/main")
                .unwrap(),
            // the item holds the declared host WITH the value
            Some(format!(
                "{{\"host\": \"api.openai.com\", \"fields\": {{\"api_key\": \"{KEY}\"}}}}"
            ))
        );
        assert!(!f.file().exists(), "no file is written");
        let got = call(&f.k, json!({"type": "get", "ref": "openai/main"}), None).await;
        assert_eq!(got.result["fields"], json!({"api_key": KEY}));
        call(&f.k, json!({"type": "forget", "ref": "openai/main"}), None).await;
        assert!(f.keychain.accounts().is_empty());
        // the reflect names the backend and no file
        let r = call(&f.k, json!({"type": "reflect"}), None).await.result;
        assert_eq!(r["backend"], json!("keychain"));
        assert!(r.get("file").is_none());
    }
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
        "refs": {"ci/key": {"host": "api.example",
                            "env": {"api_key": var}}},
    }))
    .await;
    std::env::set_var(var, KEY);
    let got = call(&f.k, json!({"type": "get", "ref": "ci/key"}), Some("alice")).await;
    assert_eq!(
        got.result,
        json!({"ref": "ci/key", "host": "api.example", "fields": {"api_key": KEY}})
    );
    let w = call(
        &f.k,
        json!({"type": "set", "ref": "ci/key", "fields": {"api_key": KEY2}}),
        None,
    )
    .await;
    assert_eq!(w.result["arg"], json!("backend"), "{}", w.result);
    std::env::remove_var(var);
    let gone = call(&f.k, json!({"type": "get", "ref": "ci/key"}), Some("alice")).await;
    assert_eq!(gone.status.as_str(), "failed", "{}", gone.result);
    assert_eq!(
        gone.result["error"],
        json!(format!(
            "LookupError: credential_store: ref 'ci/key' reads 'api_key' from ${var}, which is \
             unset"
        ))
    );
    // its listing names the fields from the env map
    let listed = call(&f.k, json!({"type": "list"}), Some("alice"))
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
    )
    .await;
    assert!(blank.result["error"]
        .as_str()
        .unwrap()
        .ends_with("not None — refused, nothing read"));
}

// ─── the host at rest ────────────────────────────────────────────────────────

/// python `_at_rest` — the `get {ref, host}` host-mismatch shape: bound to the
/// declared host, not the stored one.
fn at_rest(r: &str, stored: &str, declared: &str) -> Value {
    json!({
        "error": format!(
            "credential_store: ref {} is bound to host {}, not {} — refused, nothing read",
            py_repr(r), py_repr(declared), py_repr(stored)
        ),
        "reason": "bad_arg",
        "arg": "host",
    })
}

/// The value is stored WITH the host its declaration named at `set`. The
/// operator re-declaring the ref with another host does not carry the value
/// there: every `get` is refused, with or without a `host`, and nothing is read —
/// until a writer sets the value again, which stores it under the new host.
#[tokio::test]
async fn a_redeclared_host_reads_nothing_until_the_value_is_set_again() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    let mut entry = store_meta(&f.k, "refs")["openai/main"].clone();
    entry["host"] = json!("attacker.example");
    declare(&f.k, json!({"openai/main": entry})).await;
    for payload in [
        json!({"type": "get", "ref": "openai/main"}),
        json!({"type": "get", "ref": "openai/main", "host": "attacker.example"}),
    ] {
        let out = call(&f.k, payload, Some("alice")).await;
        assert_eq!(
            out.result,
            at_rest("openai/main", "api.openai.com", "attacker.example")
        );
        assert!(!out.result.to_string().contains(KEY));
    }
    let back = call(
        &f.k,
        json!({"type": "set", "ref": "openai/main", "fields": {"api_key": KEY2}}),
        Some("bob"),
    )
    .await;
    assert_eq!(
        back.result["host"],
        json!("attacker.example"),
        "{}",
        back.result
    );
    let got = call(
        &f.k,
        json!({"type": "get", "ref": "openai/main"}),
        Some("alice"),
    )
    .await;
    assert_eq!(
        got.result,
        json!({"ref": "openai/main", "host": "attacker.example", "fields": {"api_key": KEY2}})
    );
}

/// The shared-default case: a second store naming the same `file` declares
/// the SAME ref with another host. The value was stored for the first store's
/// host, so the second store's `get` is refused — the operator's included.
#[tokio::test]
async fn another_store_sharing_the_file_reads_nothing_for_its_host() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    let root = f.k.root().expect("root");
    let made = op(
        &f.k,
        json!({"type": "create_agent", "parent_id": root.id.0,
               "handler_module": HANDLER_MODULE, "id": "other",
               "file": f.file().to_string_lossy(),
               "refs": {"openai/main": {"host": "blocked.invalid"}}}),
    )
    .await;
    assert_eq!(made["id"], json!("other"), "{made}");
    let out =
        f.k.send(vec![Item::new(
            AgentId::from("other"),
            json!({"type": "get", "ref": "openai/main", "host": "blocked.invalid"}),
        )])
        .await
        .remove(0);
    assert_eq!(
        out.result,
        at_rest("openai/main", "api.openai.com", "blocked.invalid")
    );
    assert!(!out.result.to_string().contains(KEY));
}

/// No back-compat: a value at rest that carries no host — the old
/// `{field: value}` shape, or a hand-written entry — is refused, nothing read.
#[tokio::test]
async fn a_stored_value_without_a_host_is_refused() {
    let no_host = json!({
        "error": "credential_store: ref 'openai/main' is bound to host 'api.openai.com', \
                  not None — refused, nothing read",
        "reason": "bad_arg",
        "arg": "host",
    });
    for backend in ["file", "keychain"] {
        let f = mk(json!({"backend": backend})).await;
        declare(&f.k, json!({"openai/main": {"host": "api.openai.com"}})).await;
        if backend == "file" {
            std::fs::write(
                f.file(),
                format!("refs:\n  openai/main:\n    api_key: {KEY}\n"),
            )
            .unwrap();
            set_mode(f.file(), 0o600).unwrap();
        } else {
            f.keychain
                .set_password(
                    "fantastic",
                    "creds/openai/main",
                    &format!("{{\"api_key\": \"{KEY}\"}}"),
                )
                .unwrap();
        }
        let out = call(
            &f.k,
            json!({"type": "get", "ref": "openai/main"}),
            Some("alice"),
        )
        .await;
        assert_eq!(out.result, no_host, "{backend}");
        assert!(!out.result.to_string().contains(KEY), "{backend}");
    }
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

#[test]
fn validate_meta_texts_are_pythons() {
    let b = CredentialStoreBundle::default();
    let v = |m: Value| b.validate_meta(m.as_object().unwrap(), &Map::new());
    assert_eq!(v(json!({"backend": null})), None);
    assert_eq!(
        v(json!({"file": " "})).as_deref(),
        Some("file must be a non-empty string")
    );
    assert_eq!(
        v(json!({"keychain_namespace": 3})).as_deref(),
        Some("keychain_namespace must be a non-empty string")
    );
    assert_eq!(
        v(json!({"file": " creds.yaml"})).as_deref(),
        Some(
            "file=' creds.yaml' must be an absolute path (starting with / or ~) — a relative \
             path is refused"
        )
    );
    assert_eq!(v(json!({"file": "/abs/creds.yaml"})), None);
    assert_eq!(
        v(json!({"refs": []})).as_deref(),
        Some("refs must be a map of ref to {host, fields, env}")
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
        v(json!({"refs": {"r": {"host": "://"}}})).as_deref(),
        Some("refs['r'].host must be a non-empty host")
    );
    for nohost in [json!({}), json!({"host": null})] {
        assert_eq!(
            v(json!({"refs": {"r": nohost}})).as_deref(),
            Some("refs['r'].host is required — the one host this ref's value may be sent to")
        );
    }
    assert_eq!(
        v(json!({"refs": {"r": {"host": "h", "env": {"api_key": ""}}}})).as_deref(),
        Some("refs['r'].env must map field names to variable names")
    );
    assert_eq!(
        v(json!({"refs": {"r": {"host": "h", "fields": ["k"],
                                "env": {"k": "V"}}}})),
        None
    );
}

#[tokio::test]
async fn reflect_is_posture_never_a_value_or_a_ref_name() {
    let f = mk(json!({})).await;
    seed(&f.k).await;
    let r = call(&f.k, json!({"type": "reflect"}), None).await.result;
    let keys: Vec<&String> = r.as_object().unwrap().keys().take(5).collect();
    assert_eq!(keys, ["id", "sentence", "backend", "refs", "file"]);
    assert_eq!(r["backend"], json!("file"));
    assert_eq!(r["refs"], json!(1));
    assert_eq!(r["file"], json!(f.file().to_string_lossy()));
    assert_eq!(
        VERBS.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        ["reflect", "boot", "set", "get", "list", "forget"]
    );
    let text = r.to_string();
    assert!(!text.contains(KEY) && !text.contains("openai/main"));
    // with no `file`, the default under the user's home is NAMED, never touched
    assert_eq!(
        CredentialStoreBundle::reflect(&AgentId::from("c2"), &Map::new())["file"],
        json!(crate::default_file("c2").unwrap())
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
        let out = call(&f.k, payload, None).await.result;
        assert_eq!(
            out,
            json!({"error": err, "reason": "missing_arg", "arg": arg})
        );
    }
    let bad = call(
        &f.k,
        json!({"type": "set", "ref": "r", "fields": {"k": 1}}),
        None,
    )
    .await
    .result;
    assert_eq!(
        bad,
        json!({"error": "credential_store set: fields must be a non-empty map of field name to \
                         string value", "reason": "bad_arg", "arg": "fields"})
    );
    let unk = call(&f.k, json!({"type": "shutdown"}), None).await.result;
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
    let mut refs = Map::new();
    for i in 0..16 {
        refs.insert(format!("r{i:02}"), json!({"host": "h.example"}));
    }
    declare(&f.k, Value::Object(refs)).await;
    let items: Vec<Item> = (0..16)
        .map(|i| {
            Item::new(
                AgentId::from("creds"),
                json!({"type": "set", "ref": format!("r{i:02}"), "fields": {"api_key": format!("k{i}")}}),
            )
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

#[test]
fn the_default_file_is_under_the_homes_user_credentials() {
    // FANTASTIC_HOME unset in the test process unless a runner set it
    match std::env::var_os("FANTASTIC_HOME").filter(|v| !v.is_empty()) {
        None => assert_eq!(
            crate::default_file("c2").unwrap(),
            "~/.fantastic/user/credentials/c2.yaml"
        ),
        Some(h) => assert_eq!(
            crate::default_file("c2").unwrap(),
            format!(
                "{}/user/credentials/c2.yaml",
                std::path::Path::new(&h).display()
            )
        ),
    }
}
