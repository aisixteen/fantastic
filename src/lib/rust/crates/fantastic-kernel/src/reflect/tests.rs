//! Unit tests for [`crate::reflect`].
use crate::outcome::Item;

use super::*;
use crate::agent::Agent;
use serde_json::json;
use std::path::Path;

fn mk_agent(id: &str, parent: Option<AgentId>) -> Arc<Agent> {
    Agent::new(
        AgentId::from(id),
        None,
        parent,
        Map::new(),
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

fn mk_agent_meta(id: &str, parent: Option<AgentId>, meta: Map<String, Value>) -> Arc<Agent> {
    Agent::new(
        AgentId::from(id),
        None,
        parent,
        meta,
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

/// Mirror what `send()` does for a bare agent: identity + flags.
async fn bare_reflect(kernel: &Arc<Kernel>, agent: &Arc<Agent>, payload: &Value) -> Value {
    let mut r = reflect_identity(agent);
    apply_reflect_flags(kernel, agent, payload, &mut r)
        .await
        .expect("no roster node raised");
    r
}

const PRIMER_KEYS_GONE: [&str; 9] = [
    "transports",
    "primitive",
    "envelope",
    "universal_verb",
    "binary_protocol",
    "browser_bus",
    "well_known",
    "agent_count",
    "available_bundles",
];

#[test]
fn tree_node_includes_id_and_empty_children() {
    let a = mk_agent("leaf", Some(AgentId::from("p")));
    let v = tree_node(&a);
    assert_eq!(v["id"], "leaf");
    assert_eq!(v["parent_id"], "p");
    assert_eq!(v["children"], json!([]));
}

#[tokio::test]
async fn root_reflect_is_uniform_no_primer_keys() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = bare_reflect(&kernel, &root, &json!({"type": "reflect"})).await;
    assert_eq!(v["id"], "kernel_state");
    assert!(v["sentence"]
        .as_str()
        .unwrap()
        .starts_with("Fantastic kernel"));
    assert_eq!(v["parent_id"], Value::Null);
    // tree default = all.
    assert_eq!(v["tree"]["id"], "kernel_state");
    // bundles omitted by default.
    assert!(v.get("bundles").is_none());
    // kernel runtime identity + deployment context — root only.
    assert_eq!(v["runtime"], "rust");
    // No FANTASTIC_ENV / FANTASTIC_VERSION in the test process → host defaults.
    assert_eq!(v["env"], "host");
    assert!(v["version"].is_null());
    for k in PRIMER_KEYS_GONE {
        assert!(v.get(k).is_none(), "deleted primer key {k} still present");
    }
}

#[tokio::test]
async fn root_reflect_carries_a_32hex_guid() {
    let kernel = Arc::new(Kernel::new());
    let mut meta = Map::new();
    meta.insert(
        "guid".to_string(),
        json!("0123456789abcdef0123456789abcdef"),
    );
    let root = mk_agent_meta("kernel_state", None, meta);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = bare_reflect(&kernel, &root, &json!({"type": "reflect"})).await;
    let guid = v["guid"].as_str().expect("root reflect has a guid");
    assert_eq!(guid.len(), 32);
    assert!(guid
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
}

#[tokio::test]
async fn child_reflect_has_no_guid() {
    // guid is persistence-scoped + root-only. A child record never carries a
    // `guid` in its meta (only the root record does), so a child reflect must
    // not surface one — `apply_reflect_flags` only injects it on the root.
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = mk_agent("kid_1", Some(AgentId::from("kernel_state")));
    kernel.register(Arc::clone(&child));
    let v = bare_reflect(&kernel, &child, &json!({"type": "reflect"})).await;
    assert!(v.get("guid").is_none(), "guid must be root-only");
}

#[tokio::test]
async fn child_reflect_is_uniform() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = mk_agent("kid_1", Some(AgentId::from("kernel_state")));
    kernel.register(Arc::clone(&child));
    let v = bare_reflect(&kernel, &child, &json!({"type": "reflect"})).await;
    assert_eq!(v["id"], "kid_1");
    assert_eq!(v["parent_id"], "kernel_state");
    assert_eq!(v["tree"]["id"], "kid_1");
    // runtime + env/version are root-only — a child must not carry them.
    assert!(v.get("runtime").is_none());
    assert!(v.get("env").is_none());
    assert!(v.get("version").is_none());
    for k in PRIMER_KEYS_GONE {
        assert!(v.get(k).is_none());
    }
}

#[tokio::test]
async fn tree_tiers() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let child = mk_agent("kid_1", Some(AgentId::from("kernel_state")));
    kernel.register(Arc::clone(&child));
    root.add_child(Arc::clone(&child));
    // tree=ids → flat list, self first.
    let ids = bare_reflect(&kernel, &root, &json!({"type": "reflect", "tree": "ids"})).await;
    assert_eq!(ids["tree"], json!(["kernel_state", "kid_1"]));
    // tree=none → omitted.
    let none = bare_reflect(&kernel, &root, &json!({"type": "reflect", "tree": "none"})).await;
    assert!(none.get("tree").is_none());
}

#[tokio::test]
async fn bundles_tiers() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // No bundles registered in this bare kernel → empty list (not absent).
    let all = bare_reflect(
        &kernel,
        &root,
        &json!({"type": "reflect", "bundles": "all"}),
    )
    .await;
    assert!(all["bundles"].is_array());
    let ids = bare_reflect(
        &kernel,
        &root,
        &json!({"type": "reflect", "bundles": "ids"}),
    )
    .await;
    assert!(ids["bundles"].is_array());
}

#[tokio::test]
async fn description_surfaces_top_and_tree() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let mut meta = Map::new();
    meta.insert("description".to_string(), json!("holds my notes"));
    let child = mk_agent_meta("kid_1", Some(AgentId::from("kernel_state")), meta);
    kernel.register(Arc::clone(&child));
    root.add_child(Arc::clone(&child));
    // top-level on the child's own reflect.
    let own = bare_reflect(&kernel, &child, &json!({"type": "reflect"})).await;
    assert_eq!(own["description"], "holds my notes");
    // and in the parent's tree=all node.
    let root_v = bare_reflect(&kernel, &root, &json!({"type": "reflect"})).await;
    let node = root_v["tree"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "kid_1")
        .unwrap();
    assert_eq!(node["description"], "holds my notes");
}

#[tokio::test]
async fn readme_flag_tiers() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // default → no readme key.
    let plain = bare_reflect(&kernel, &root, &json!({"type": "reflect"})).await;
    assert!(plain.get("readme").is_none());
    // readme=true with no file on disk → key present, value null.
    let r = bare_reflect(&kernel, &root, &json!({"type": "reflect", "readme": true})).await;
    assert!(r.get("readme").is_some());
    assert_eq!(r["readme"], Value::Null);
}

// ─── keyed / tiered doc tree ───────────────────────────────────

// The `self_extension` pointer and the always-on doc FRAME (`readmes`,
// `docs`, `persistence`, `guid`) are the ROOT LOADER's contribution now —
// `Bundle::reflect_root_extra`, merged by `apply_reflect_flags` on the root.
// Their tests live with the bundle that owns the text
// (`crates/bundles/fantastic-core/src/tests.rs`).

#[test]
fn est_tokens_matches_ceil_div_and_desc_prefers_quote() {
    assert_eq!(est_tokens(&"x".repeat(400)), 100); // 400/4
    assert_eq!(est_tokens(&"x".repeat(401)), 101); // ceil
    assert_eq!(est_tokens(""), 0);
    assert_eq!(read_desc("> the desc\n\n# Title\nbody"), "the desc");
    assert_eq!(read_desc("# Title\n\nfirst prose line"), "first prose line");
    assert_eq!(read_desc("# only a heading"), "");
}

/// Seed a minimal orientation + docs tree (with one nested tier) at `<base>`.
fn seed_docs(base: &Path) {
    let docs = base.join("docs");
    std::fs::create_dir_all(docs.join("frontend")).unwrap();
    std::fs::write(
        base.join("readme.md"),
        "# This is a Fantastic kernel.\n\nkeyed doc tree — reflect readme=index",
    )
    .unwrap();
    std::fs::write(
        docs.join("self-extension.md"),
        "> author your own tools\n\n# Self-extension\nauthor durable tools here",
    )
    .unwrap();
    std::fs::write(
        docs.join("streams.md"),
        "> byte streams\n\n# Streams\nchunked bytes",
    )
    .unwrap();
    std::fs::write(
        docs.join("frontend.md"),
        "> two kernels\n\n# Frontend\nhost + browser",
    )
    .unwrap();
    std::fs::write(
        docs.join("frontend").join("routing.md"),
        "> routing rule\n\n# Routing\nonly the js kernel",
    )
    .unwrap();
}

#[tokio::test]
async fn readme_doctree_frame_index_view_and_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join(".fantastic");
    seed_docs(&base);
    let kernel = Arc::new(Kernel::new());
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        base,
        true,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    // readme=true → the SHORT orientation string.
    let r = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":true}),
    )
    .await;
    assert!(r["readme"].as_str().unwrap().contains("Fantastic kernel"));

    // readme=index → token-annotated tree; nesting preserved; NO body.
    let idx = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"index"}),
    )
    .await;
    let arr = idx["readme"]["index"].as_array().unwrap();
    for e in arr {
        assert!(e.get("body").is_none());
        assert!(e["tokens"].as_u64().unwrap() > 0);
    }
    let fe = arr.iter().find(|e| e["key"] == "frontend").unwrap();
    assert_eq!(fe["children"][0]["key"], "frontend/routing");

    // readme=<key> → body + (leaf) no children.
    let doc = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"self-extension"}),
    )
    .await;
    assert_eq!(doc["readme"]["key"], "self-extension");
    assert!(doc["readme"]["body"].as_str().unwrap().contains("author"));

    // A nested key is reachable by its slashed path.
    let nested = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"frontend/routing"}),
    )
    .await;
    assert_eq!(nested["readme"]["key"], "frontend/routing");

    // Unknown key → {error, keys}; a `..` escape is refused.
    let miss = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"nope"}),
    )
    .await;
    assert!(miss["readme"]["error"].is_string());
    assert!(miss["readme"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .any(|k| k == "self-extension"));
    let esc = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"../readme"}),
    )
    .await;
    assert_eq!(esc["readme"]["error"], "bad key '../readme'");
}

/// A12.2 — `minds` was a rust-only topic python never shipped. With it gone the
/// key is an ORDINARY unknown key: the reply stays well-formed and names the keys
/// that do exist, rather than serving a doc no other runtime has.
#[tokio::test]
async fn readme_minds_is_no_doc() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join(".fantastic");
    seed_docs(&base);
    let kernel = Arc::new(Kernel::new());
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        base,
        true,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let r = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"minds"}),
    )
    .await;
    assert_eq!(r["readme"]["error"], "no doc 'minds'");
    let keys = r["readme"]["keys"].as_array().unwrap();
    assert!(keys.iter().any(|k| k == "self-extension"));
    assert!(!keys.iter().any(|k| k == "minds"));
}

#[tokio::test]
async fn non_root_has_no_readmes_frame() {
    let kernel = Arc::new(Kernel::new());
    let child = mk_agent("kid", Some(AgentId::from("kernel_state")));
    kernel.register(Arc::clone(&child));
    let r = bare_reflect(&kernel, &child, &json!({"type":"reflect","tree":"none"})).await;
    assert!(r.get("readmes").is_none());
    assert!(r.get("docs").is_none());
}

/// Seed a docs tree exercising the R2-RFL-5 clamp/existence matrix: a REAL
/// nested dir (`nested/`), a topic that is a FILE sitting where a later `..`
/// wants to step through it as a dir (`frontend/panel-recipe.md`, no
/// `frontend/panel-recipe/` dir), and a top-level topic (`topic.md`).
fn seed_clamp_docs(base: &Path) {
    let docs = base.join("docs");
    std::fs::create_dir_all(docs.join("frontend")).unwrap();
    std::fs::create_dir_all(docs.join("nested")).unwrap();
    std::fs::write(
        docs.join("frontend").join("routing.md"),
        "> r\n\nrouting body",
    )
    .unwrap();
    std::fs::write(
        docs.join("frontend").join("panel-recipe.md"),
        "> p\n\npanel body",
    )
    .unwrap();
    std::fs::write(docs.join("nested").join("child.md"), "> c\n\nchild body").unwrap();
    std::fs::write(docs.join("topic.md"), "> t\n\ntopic body").unwrap();
}

fn mk_clamp_kernel(base: std::path::PathBuf) -> (Arc<Kernel>, Arc<Agent>) {
    let kernel = Arc::new(Kernel::new());
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        base,
        true,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    (kernel, root)
}

/// C33 (python 2026-09-20): the key check is LEXICAL and then the INDEX — a
/// `.`, `..` or empty segment is a `bad key` whatever the filesystem holds, and
/// a key is a doc only if the index lists it; the disk is never asked.
#[tokio::test]
async fn readme_view_key_check_is_lexical_then_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join(".fantastic");
    seed_clamp_docs(&base);
    let (kernel, root) = mk_clamp_kernel(base);

    // `nested/../topic`: a `..` segment is a bad key even though `nested` is a
    // real directory and the target exists.
    let r = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"nested/../topic"}),
    )
    .await;
    assert_eq!(r["readme"]["error"], "bad key 'nested/../topic'");

    // `frontend/panel-recipe/../routing`: the same, whether or not
    // `panel-recipe` is a directory — the filesystem is never consulted.
    let r = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"frontend/panel-recipe/../routing"}),
    )
    .await;
    assert_eq!(
        r["readme"]["error"],
        "bad key 'frontend/panel-recipe/../routing'"
    );

    // `nested//child`: an EMPTY segment is a bad key (no POSIX collapsing).
    let r = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"nested//child"}),
    )
    .await;
    assert_eq!(r["readme"]["error"], "bad key 'nested//child'");

    // A leading `\` becomes `/` (backslash→slash) AFTER the strip, so
    // `\topic` is `/topic` — an empty first segment. `bad key`,
    // py_repr-quoted, echoing the ORIGINAL `sel`.
    let sel = "\\topic";
    let r = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":sel}),
    )
    .await;
    assert_eq!(
        r["readme"]["error"],
        format!("bad key {}", fantastic_bundle::py_repr(sel))
    );

    // `/` alone strips to an empty `safe` — a bad key.
    let r = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"/"}),
    )
    .await;
    assert_eq!(r["readme"]["error"], "bad key '/'");
}

/// C59: doc reads translate `\r\n` and a lone `\r` to `\n` (python text mode)
/// BEFORE `tokens`/`desc` are computed from the body — a doc kept with its
/// CRs counts one extra "character" per CRLF pair and desc-matching sees
/// stray `\r`s python's reader never produces.
#[tokio::test]
async fn doc_reads_translate_newlines_like_python_text_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join(".fantastic");
    let docs = base.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    // 4 chars after translation ("a\nb\nc\n" minus... let's just measure): raw
    // bytes below are "> desc\r\nbody\rline\r\n" (CRLF, lone CR, CRLF).
    std::fs::write(docs.join("crlf.md"), b"> desc\r\nbody\rline\r\n").unwrap();
    let (kernel, root) = mk_clamp_kernel(base);

    let r = bare_reflect(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"crlf"}),
    )
    .await;
    let body = r["readme"]["body"].as_str().unwrap();
    assert!(
        !body.contains('\r'),
        "CRs must be translated away: {body:?}"
    );
    assert_eq!(body, "> desc\nbody\nline\n");
    // python `len("> desc\nbody\nline\n")` == 18 code points, ceil(18/4) == 5 —
    // NOT the raw-byte count (20, which a CR-preserving reader would report).
    assert_eq!(r["readme"]["tokens"], 5);
    assert_eq!(r["readme"]["desc"], "desc");
}

/// C60: python catches only `OSError` around a doc read, so a
/// `UnicodeDecodeError` is NOT "unreadable" — it propagates out of the verb
/// and fails the WHOLE reflect, `{error: "UnicodeDecodeError: …", raised:
/// true}`, exactly like any other raised handler.
#[tokio::test]
async fn a_non_utf8_doc_fails_the_reflect_like_python() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join(".fantastic");
    let docs = base.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    // 0xff can never start a UTF-8 sequence — CPython's own worked example.
    std::fs::write(
        docs.join("bad.md"),
        [b'x', b'x', b'x', b'x', b'x', b'x', 0xff],
    )
    .unwrap();
    let (kernel, root) = mk_clamp_kernel(base);

    let mut reply = reflect_identity(&root);
    let err = apply_reflect_flags(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":"bad"}),
        &mut reply,
    )
    .await
    .expect_err("a non-UTF-8 doc must raise, not answer 'unreadable'");
    assert_eq!(
        err["error"],
        "UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff in position \
         6: invalid start byte"
    );
    assert_eq!(err["raised"], true);

    // Same for the top-level `readme.md` orientation file (`readme=true`).
    std::fs::write(root.root_path.join("readme.md"), [0xff, b'x']).unwrap();
    let mut reply2 = reflect_identity(&root);
    let err2 = apply_reflect_flags(
        &kernel,
        &root,
        &json!({"type":"reflect","tree":"none","readme":true}),
        &mut reply2,
    )
    .await
    .expect_err("readme=true must raise the same way");
    assert!(err2["error"]
        .as_str()
        .unwrap()
        .starts_with("UnicodeDecodeError:"));
}

// ─── #1 / #7 — paged listings + the gate made readable ──────────────

#[tokio::test]
async fn tree_ids_is_paged_and_the_cursor_walks_the_whole_tree() {
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    for i in 0..5 {
        let c = mk_agent(&format!("kid_{i}"), Some(root.id.clone()));
        kernel.register(Arc::clone(&c));
        root.add_child(Arc::clone(&c));
    }

    // Default is UNCHANGED — everything, no cursor.
    let all = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"reflect","tree":"ids"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(all["tree"].as_array().unwrap().len(), 6); // self + 5
                                                          // C8: no further page ⇒ no cursor key at all (python inserts `next` only
                                                          // when one exists; there is no `tree_next`).
    assert!(all.get("next").is_none(), "{all}");
    assert!(all.get("tree_next").is_none(), "{all}");

    // Bounded, and the cursor names the last id handed over.
    let p1 = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type":"reflect","tree":"ids","limit":2}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(p1["tree"].as_array().unwrap().len(), 2);
    let cursor = p1["next"].as_str().expect("a further page exists");
    assert_eq!(cursor, p1["tree"][1].as_str().unwrap());

    // Resuming visits each id exactly once.
    let mut seen: Vec<String> = p1["tree"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let mut cur = cursor.to_string();
    loop {
        let p = kernel
            .send(vec![Item::new(
                AgentId::from("kernel"),
                json!({"type":"reflect","tree":"ids","limit":2,"after":cur}),
            )])
            .await
            .remove(0)
            .result;
        for v in p["tree"].as_array().unwrap() {
            seen.push(v.as_str().unwrap().to_string());
        }
        match p["next"].as_str() {
            Some(c) => cur = c.to_string(),
            None => break,
        }
    }
    assert_eq!(seen.len(), 6);
    let mut uniq = seen.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), 6, "a paged walk must not repeat or skip");
}

#[tokio::test]
async fn an_agent_can_read_its_own_authority() {
    // The gate used to be INVISIBLE: an agent could only discover its authority
    // by trying, which cost whole passes of empirical probing every run. It also
    // makes `update_agent`'s replace-not-merge warning actionable — you must
    // send the map whole, so you must be able to read it.
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let mut meta = Map::new();
    meta.insert("can_call".into(), json!({"store": ["read"]}));
    meta.insert("callable_by".into(), json!({"cli": "*"}));
    let a = mk_agent_meta("worker", Some(root.id.clone()), meta);
    kernel.register(Arc::clone(&a));
    root.add_child(Arc::clone(&a));

    let own = kernel
        .send(vec![Item::new(a.id.clone(), json!({"type": "reflect"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(own["can_call"]["store"][0], "read");
    assert_eq!(own["callable_by"]["cli"], "*");

    // ...and the same two keys ride every WIRING tree node (`detail:full`), so
    // the whole authorization landscape is readable from ONE root reflect. The
    // distilled roster carries purpose, not posture (python `_PURPOSE_KEYS`).
    let tree = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect", "detail": "full"}),
        )])
        .await
        .remove(0)
        .result;
    let node = tree["tree"]["children"]
        .as_array()
        .and_then(|c| c.iter().find(|n| n["id"] == "worker"))
        .expect("the worker node");
    assert_eq!(node["can_call"]["store"][0], "read");
    assert_eq!(node["callable_by"]["cli"], "*");
}

// ─── C5 — children walk CREATION order ─────────────────────────

#[tokio::test]
async fn tree_children_keep_creation_order() {
    // Python's `_children` is a plain dict, so the tree frame, `tree=ids` and
    // the descendant walk all read insertion order. Rust's `DashMap` had none,
    // so every reader sorted by id and printed a tree python never prints —
    // measured in `root_reflect_full.json` children and `prompt_req_01.json`.
    let kernel = Arc::new(Kernel::new());
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    // Deliberately reverse-alphabetical: a sort would invert this.
    for id in ["zulu", "mike", "alpha"] {
        let c = mk_agent(id, Some(root.id.clone()));
        kernel.register(Arc::clone(&c));
        root.add_child(Arc::clone(&c));
    }
    // A grandchild under the LAST child, to pin the DFS order too.
    let g = mk_agent("gamma", Some(AgentId::from("alpha")));
    kernel.register(Arc::clone(&g));
    kernel
        .agents
        .get(&AgentId::from("alpha"))
        .unwrap()
        .add_child(Arc::clone(&g));

    // `tree` frame.
    let node = tree_node(&root);
    let kids: Vec<&str> = node["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(kids, vec!["zulu", "mike", "alpha"], "{node}");

    // `tree=ids` / descendant walk — DFS, self first, children in creation order.
    assert_eq!(
        descendant_ids(&root),
        vec!["kernel_state", "zulu", "mike", "alpha", "gamma"]
    );

    // `child_ids()` is the one ordered accessor both read through.
    assert_eq!(
        root.child_ids(),
        vec![
            AgentId::from("zulu"),
            AgentId::from("mike"),
            AgentId::from("alpha")
        ]
    );

    // A detach leaves the survivors in place — it does not re-sort.
    root.remove_child(&AgentId::from("mike"));
    assert_eq!(
        root.child_ids(),
        vec![AgentId::from("zulu"), AgentId::from("alpha")]
    );
}

// ─── C1 + C7 + C8 + C58 — the reflect flags are python's ─────────────

/// python `_MORE_HINT`, verbatim.
const PY_MORE_HINT: &str = "Distilled: every agent and what it is FOR. Reflect one agent for its verbs (<type>reflect</type>). Manual: <type>reflect</type> <readme>index</readme> - keys include system-verbs (ps/cancel/create_agent args) and self-extension (author your own durable tools). <detail>full</detail> on a reflect adds wiring and posture - ask for it only when you need to see how agents are plumbed, not to find one.";

/// A bundle whose reflect names a sentence and a `mode` (or raises, for the
/// agent named `boom`).
struct RosterBundle;

#[async_trait::async_trait]
impl crate::bundle::Bundle for RosterBundle {
    fn name(&self) -> &str {
        "roster_probe"
    }
    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        if payload.get("type").and_then(Value::as_str) != Some("reflect") {
            return Ok(Some(json!({"ok": true})));
        }
        if agent_id.as_str() == "boom" {
            return Err("RuntimeError: reflect exploded".into());
        }
        Ok(Some(json!({
            "id": agent_id.as_str(),
            "sentence": format!("{} is a roster probe", agent_id.as_str()),
            "handler_module": "roster_probe.tools",
            "mode": if agent_id.as_str() == "modeless" { Value::Null } else { json!("watch") },
        })))
    }
}

/// A kernel with `roster_probe.tools` INSTALLED and `fixture_probe.tools`
/// registered UNLISTED, and a root `kernel_state`.
fn roster_kernel() -> (Arc<Kernel>, Arc<Agent>) {
    let mut kernel = Kernel::new();
    let mut reg = crate::bundle::BundleRegistry::new();
    reg.register("roster_probe.tools", RosterBundle);
    reg.register_unlisted("fixture_probe.tools", RosterBundle);
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let mut meta = Map::new();
    meta.insert(
        "callable_by".into(),
        json!({"$self": "*", "$parent": "*", "$children": "*"}),
    );
    let root = mk_agent_meta("kernel_state", None, meta);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    (kernel, root)
}

/// A handler-bearing child of `parent` with `meta`.
fn probe_child(kernel: &Arc<Kernel>, id: &str, parent: &Arc<Agent>, meta: Value) -> Arc<Agent> {
    let Value::Object(meta) = meta else {
        panic!("meta must be an object")
    };
    let a = Agent::new(
        AgentId::from(id),
        Some("roster_probe.tools".to_string()),
        Some(parent.id.clone()),
        meta,
        Path::new("/tmp/nowhere").join(id),
        true,
    );
    kernel.register(Arc::clone(&a));
    parent.add_child(Arc::clone(&a));
    a
}

async fn kernel_reflect(kernel: &Arc<Kernel>, payload: Value) -> crate::outcome::Outcome {
    kernel
        .send(vec![Item::new(AgentId::from("kernel"), payload)])
        .await
        .remove(0)
}

fn find<'a>(node: &'a Value, id: &str) -> Option<&'a Value> {
    if node["id"] == id {
        return Some(node);
    }
    node["children"]
        .as_array()?
        .iter()
        .find_map(|c| find(c, id))
}

#[tokio::test]
async fn distilled_reflect_carries_more_hint_verbatim() {
    let (kernel, root) = roster_kernel();
    let child = probe_child(&kernel, "kid", &root, json!({}));
    assert_eq!(MORE_HINT, PY_MORE_HINT);

    // Every distilled reflect — the root, a bundle agent, a bare agent.
    let r = kernel_reflect(&kernel, json!({"type": "reflect"}))
        .await
        .result;
    assert_eq!(r["more"], json!(PY_MORE_HINT));
    let c = kernel
        .send(vec![Item::new(
            child.id.clone(),
            json!({"type": "reflect", "tree": "none"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(c["more"], json!(PY_MORE_HINT), "{c}");

    // Python order: after `tree`/`next`, before `bundles`/`readme`.
    let r = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "ids", "limit": 1, "bundles": "ids", "readme": true}),
    )
    .await
    .result;
    let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
    let tail: Vec<&str> = keys[keys.iter().position(|k| *k == "tree").unwrap()..].to_vec();
    assert_eq!(tail, ["tree", "next", "more", "bundles", "readme"], "{r}");

    // `detail:full` carries none.
    let full = kernel_reflect(&kernel, json!({"type": "reflect", "detail": "full"}))
        .await
        .result;
    assert!(full.get("more").is_none(), "{full}");
}

#[tokio::test]
async fn distilled_tree_is_the_gated_roster() {
    // py `test_reflect_roster_gate.py::test_the_roster_omits_what_the_caller_may_not_reflect`
    let (kernel, root) = roster_kernel();
    let visible = probe_child(
        &kernel,
        "rg_visible",
        &root,
        json!({
            "description": "the visible one",
            "callable_by": {"$self": "*", "$parent": "*", "$children": "*", "rg_reader": ["reflect"]},
        }),
    );
    let secret = probe_child(
        &kernel,
        "rg_secret",
        &visible,
        json!({"description": "the secret one"}),
    );
    let _modeless = probe_child(&kernel, "modeless", &root, json!({}));
    let reader = probe_child(
        &kernel,
        "rg_reader",
        &root,
        json!({
            "can_call": {"$self": "*", "$parent": "*", "$children": "*", "kernel_state": ["reflect"], "rg_visible": ["reflect"]},
            "callable_by": {"$self": "*", "$parent": "*", "$children": "*"},
        }),
    );

    let r = crate::send::with_sender(
        reader.id.clone(),
        kernel_reflect(&kernel, json!({"type": "reflect"})),
    )
    .await
    .result;
    let tree = &r["tree"];
    let seen = find(tree, "rg_visible").expect("visible node");
    // PURPOSE, not wiring: the node's own sentence + mode, the operator's
    // description, and nothing of the record.
    assert_eq!(
        seen,
        &json!({
            "id": "rg_visible",
            "sentence": "rg_visible is a roster probe",
            "description": "the visible one",
            "mode": "watch",
            "children": [{"id": "rg_secret", "children": []}],
        }),
        "{tree}"
    );
    let hidden = find(tree, secret.id.as_str()).expect("the SHAPE stays public");
    assert!(hidden.get("description").is_none(), "{hidden}");
    assert!(hidden.get("sentence").is_none(), "{hidden}");
    // The bare root answers its identity; a null `mode` is not copied.
    assert_eq!(
        tree["sentence"],
        "Fantastic kernel. Everything is reachable by sending messages to agents."
    );
    assert!(tree.get("parent_id").is_none() && tree.get("handler_module").is_none());
    let modeless = find(tree, "modeless").unwrap();
    assert!(modeless.get("mode").is_none(), "{modeless}");

    // The operator sees both, as everywhere else.
    let op = kernel_reflect(&kernel, json!({"type": "reflect"}))
        .await
        .result;
    assert_eq!(
        find(&op["tree"], "rg_secret").unwrap()["description"],
        "the secret one"
    );

    // `detail:full` keeps the WIRING node.
    let full = kernel_reflect(&kernel, json!({"type": "reflect", "detail": "full"}))
        .await
        .result;
    let wired = find(&full["tree"], "rg_visible").unwrap();
    assert_eq!(wired["handler_module"], "roster_probe.tools");
    assert_eq!(wired["parent_id"], "kernel_state");
    assert!(wired.get("sentence").is_none(), "{wired}");
}

#[tokio::test]
async fn distilled_tree_reflect_echoes_to_node_watchers() {
    // The per-node reflect is a REAL dispatch: the node's watchers hear
    // `{type:'reflect'}`, and an `internal` call row is written that nothing
    // settles. Python `_run_dispatch`.
    let (kernel, root) = roster_kernel();
    let node = probe_child(&kernel, "watched", &root, json!({}));
    let watcher = mk_agent("tap", Some(root.id.clone()));
    kernel.register(Arc::clone(&watcher));
    root.add_child(Arc::clone(&watcher));
    let mut rx = kernel.ensure_inbox(&watcher.id);
    assert!(kernel.watch(&node.id, watcher.id.clone()).await.is_none());

    let out = kernel_reflect(&kernel, json!({"type": "reflect"})).await;
    assert_eq!(
        rx.try_recv().expect("the roster reflect echoed").payload,
        json!({"type": "reflect"})
    );
    assert!(rx.try_recv().is_none(), "one echo per reflect");

    let rows = crate::test_support::rows_about(&kernel, &node.id);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(
        row.keys().map(String::as_str).collect::<Vec<_>>(),
        ["id", "kind", "verb", "parent", "caller", "payload", "internal", "agent", "ts"],
        "no outcome stamp: the internal dispatch opens no execution"
    );
    assert_eq!(row["kind"], "call");
    assert_eq!(row["verb"], "reflect");
    assert_eq!(row["internal"], true);
    assert_eq!(row["caller"], Value::Null, "an operator reflect");
    assert_ne!(row["id"].as_i64(), out.pid.map(crate::TaskId::get));
    assert_eq!(
        crate::test_support::rows_about(&kernel, &watcher.id).len(),
        0,
        "a bare node is not dispatched"
    );

    // `detail:full` walks the wiring tree — nothing is dispatched.
    let _ = kernel_reflect(&kernel, json!({"type": "reflect", "detail": "full"})).await;
    assert!(rx.try_recv().is_none());
    assert_eq!(crate::test_support::rows_about(&kernel, &node.id).len(), 1);
}

#[tokio::test]
async fn a_raising_roster_node_fails_the_reflect() {
    // python: the exception propagates out of `_roster` and `_settle_once`
    // records the whole reflect `failed`.
    let (kernel, root) = roster_kernel();
    let _boom = probe_child(&kernel, "boom", &root, json!({}));
    let out = kernel_reflect(&kernel, json!({"type": "reflect"})).await;
    assert_eq!(out.status, Status::Failed);
    assert_eq!(
        out.result,
        json!({"error": "RuntimeError: reflect exploded", "raised": true})
    );
    // The flat index dispatches nothing, so it still answers.
    let ids = kernel_reflect(&kernel, json!({"type": "reflect", "tree": "ids"})).await;
    assert_eq!(ids.status, Status::Completed);
}

#[tokio::test]
async fn a_non_string_tree_flag_omits_the_tree() {
    let (kernel, _root) = roster_kernel();
    let absent = kernel_reflect(&kernel, json!({"type": "reflect"}))
        .await
        .result;
    assert!(absent["tree"].is_object(), "absent ⇒ all");
    for flag in [
        Value::Null,
        json!(true),
        json!(false),
        json!(1),
        json!([]),
        json!({}),
        json!("none"),
        json!("ALL"),
        json!(""),
    ] {
        let r = kernel_reflect(&kernel, json!({"type": "reflect", "tree": flag.clone()}))
            .await
            .result;
        assert!(r.get("tree").is_none(), "tree={flag}: {r}");
        assert!(r.get("next").is_none());
        assert_eq!(r["more"], json!(PY_MORE_HINT));
    }
}

/// A bundle that only carries a catalog name.
struct Named(&'static str);

#[async_trait::async_trait]
impl crate::bundle::Bundle for Named {
    fn name(&self) -> &str {
        self.0
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        _payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<crate::bundle::Reply, crate::bundle::BundleError> {
        Ok(None)
    }
}

#[tokio::test]
async fn bundles_page_like_python() {
    let mut kernel = Kernel::new();
    let mut reg = crate::bundle::BundleRegistry::new();
    reg.register("charlie.tools", Named("charlie"));
    reg.register("alpha.tools", Named("alpha"));
    reg.register("bravo.tools", Named("bravo"));
    kernel.bundles = reg;
    let kernel = Arc::new(kernel);
    let root = mk_agent("kernel_state", None);
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    for id in ["k1", "k2"] {
        let c = mk_agent(id, Some(root.id.clone()));
        kernel.register(Arc::clone(&c));
        root.add_child(c);
    }
    let all = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "none", "bundles": "all"}),
    )
    .await
    .result;
    assert_eq!(
        all["bundles"],
        json!([
            {"name": "alpha", "handler_module": "alpha.tools"},
            {"name": "bravo", "handler_module": "bravo.tools"},
            {"name": "charlie", "handler_module": "charlie.tools"},
        ])
    );
    assert!(
        all.get("bundles_next").is_none(),
        "no further page ⇒ no key: {all}"
    );

    // `bundles=ids` pages by name with the SAME `limit`/`after` the flat tree
    // index uses in the same reflect.
    let p1 = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "ids", "bundles": "ids", "limit": 2}),
    )
    .await
    .result;
    assert_eq!(p1["tree"], json!(["kernel_state", "k1"]));
    assert_eq!(p1["next"], "k1");
    assert_eq!(p1["bundles"], json!(["alpha", "bravo"]));
    assert_eq!(p1["bundles_next"], "bravo");

    let p2 = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "ids", "bundles": "ids", "limit": 2, "after": "k1"}),
    )
    .await
    .result;
    assert_eq!(p2["tree"], json!(["k2"]));
    assert!(p2.get("next").is_none(), "{p2}");
    // `k1` names no bundle ⇒ the catalog cursor is gone ⇒ an empty tail.
    assert_eq!(p2["bundles"], json!([]));
    assert!(p2.get("bundles_next").is_none(), "{p2}");

    // `bundles=all` pages too, its cursor the entry's name.
    let pa = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "none", "bundles": "all", "limit": 1}),
    )
    .await
    .result;
    assert_eq!(
        pa["bundles"],
        json!([{"name": "alpha", "handler_module": "alpha.tools"}])
    );
    assert_eq!(pa["bundles_next"], "alpha");
    let pb = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "none", "bundles": "ids", "limit": 5, "after": "alpha"}),
    )
    .await
    .result;
    assert_eq!(pb["bundles"], json!(["bravo", "charlie"]));
    assert!(pb.get("bundles_next").is_none(), "{pb}");
}

#[tokio::test]
async fn a_probe_registered_bundle_is_absent_from_the_catalog() {
    // C10: python lists entry-point bundles only; a fixture imported without
    // one dispatches but is not installable.
    let (kernel, root) = roster_kernel();
    let ids = kernel_reflect(
        &kernel,
        json!({"type": "reflect", "tree": "none", "bundles": "all"}),
    )
    .await
    .result;
    assert_eq!(
        ids["bundles"],
        json!([{"name": "roster_probe", "handler_module": "roster_probe.tools"}])
    );
    // ...and it still dispatches.
    let fixture = Agent::new(
        AgentId::from("fx"),
        Some("fixture_probe.tools".to_string()),
        Some(root.id.clone()),
        Map::new(),
        Path::new("/tmp/nowhere").join("fx"),
        true,
    );
    kernel.register(Arc::clone(&fixture));
    let r = kernel
        .send(vec![Item::new(fixture.id.clone(), json!({"type": "ping"}))])
        .await
        .remove(0)
        .result;
    assert_eq!(r, json!({"ok": true}));
}

// ─── C11 — an empty display_name is no name ─────────────────────────

#[tokio::test]
async fn empty_display_name_falls_back_to_id() {
    let mut meta = Map::new();
    meta.insert("display_name".into(), json!(""));
    let a = mk_agent_meta("nameless", Some(AgentId::from("kernel_state")), meta);
    assert_eq!(reflect_identity(&a)["display_name"], "nameless");
    assert_eq!(tree_node(&a)["display_name"], "nameless");

    let mut meta = Map::new();
    meta.insert("display_name".into(), json!("Named"));
    let b = mk_agent_meta("named", Some(AgentId::from("kernel_state")), meta);
    assert_eq!(reflect_identity(&b)["display_name"], "Named");
    assert_eq!(tree_node(&b)["display_name"], "Named");
}

/// C12: the TREE ROOT answers `reflect` with the substrate identity even when
/// it carries a handler_module (the root loader). Its persistence verbs still
/// route to the bundle below; `reflect <root>` is the kernel's own identity.
#[tokio::test]
async fn root_with_handler_reflects_identity() {
    let mut kernel = Kernel::new();
    crate::test_support::register_root_loader(&mut kernel.bundles);
    let kernel = Arc::new(kernel);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        Some("kernel_state.tools".to_string()),
        None,
        Map::new(),
        std::path::PathBuf::new(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));
    let v = kernel
        .send(vec![crate::Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect", "tree": "none"}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(v["id"], json!("kernel_state"));
    assert_eq!(v["handler_module"], json!("kernel_state.tools"));
    assert_eq!(
        v["sentence"],
        json!("Fantastic kernel. Everything is reachable by sending messages to agents.")
    );
    // A NON-root agent of the same bundle does NOT short-circuit — its reflect
    // is its handler's, which the stand-in loader does not answer.
    let _ = kernel
        .send(vec![crate::Item::new(
            AgentId::from("kernel"),
            json!({"type":"create_agent","parent_id":"kernel","handler_module":"kernel_state.tools","id":"session"}),
        )])
        .await;
    let v = kernel
        .send(vec![crate::Item::new(
            AgentId::from("session"),
            json!({"type": "reflect"}),
        )])
        .await
        .remove(0)
        .result;
    assert_ne!(
        v["sentence"],
        json!("Fantastic kernel. Everything is reachable by sending messages to agents.")
    );
}
