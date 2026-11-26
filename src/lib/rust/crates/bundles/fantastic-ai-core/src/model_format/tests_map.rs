//! MAP ONCE: the one mechanism from stored rows to the model's view. The assembly selects
//! pieces with facts and writes no model text; the map writes every model-visible text
//! from the type's declaration ([`super::Decl`]).

use std::sync::Arc;

use fantastic_kernel::test_support::as_agent;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use tempfile::TempDir;

use super::map::{
    canonical, message, messages, pieces_of, row, system, Env, FoldKind, Howto, ImageForm, Piece,
    Shown, SysBody, SysPiece,
};
use super::{of, Decl, Out, Section, DEFAULT, REGISTRY};
use crate::tests::{mk_kernel, opens_with_a_time};

/// 2026-11-19 19:40:00 UTC.
const T1: i64 = 1_795_117_200;
/// 2026-11-24 19:40:00 UTC.
const T2: i64 = T1 + 5 * 86_400;

/// `rows` saved through the chat's append.
async fn save(kernel: &Arc<Kernel>, agent: &AgentId, rows: &[Value]) -> Vec<i64> {
    as_agent(
        agent.clone(),
        crate::memory::save_history(agent, kernel, rows),
    )
    .await
    .expect("rows saved")
}

/// The next turn's messages in flight, as the assembly builds them.
async fn assembled(
    kernel: &Arc<Kernel>,
    agent: &AgentId,
    stamp: &crate::row_attrs::Stamp,
) -> Vec<Value> {
    let state = crate::state::state_for(agent);
    as_agent(
        agent.clone(),
        crate::assembly::assemble_messages(
            agent,
            &state,
            "next",
            kernel,
            "c",
            crate::caching::Caching::ProviderCache,
            stamp,
        ),
    )
    .await
    .expect("assemble")
    .0
}

/// A stamp at `ts`, UTC.
fn stamp_at(ts: i64) -> crate::row_attrs::Stamp {
    crate::row_attrs::Stamp {
        ts: json!(ts),
        offset: Some(json!(0)),
    }
}

/// A row with thinking.
fn thinking(content: &str, chars: usize) -> Value {
    json!({"role": "assistant", "content": content, "thinking": "t".repeat(chars)})
}

/// The text of a mapped message.
fn text(m: &Value) -> &str {
    m["content"].as_str().unwrap_or("")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_assistant_message_is_its_stored_text_only() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "map_own_text").await;
    let big = format!(
        "[reply to 3 · notes read]\nhead\n{}",
        "x".repeat(crate::constants::AUTOFOLD_AT * 4 + 40)
    );
    let mut answer = thinking("I read it.", 400);
    answer["attributes"] = json!({"ts": T1, "utc_offset": 7200, "keep": {"3": "the gist"}});
    save(
        &kernel,
        &agent,
        &[
            json!({"role": "user", "content": "go", "attributes": {"ts": T1}}),
            answer,
            json!({"role": "user", "content": big}),
            thinking("done", 40),
        ],
    )
    .await;
    let inflight = assembled(&kernel, &agent, &stamp_at(T2)).await;
    for decl in REGISTRY {
        let sent = messages(decl, agent.as_str(), &inflight);
        let mine: Vec<&str> = sent
            .iter()
            .filter(|m| m["role"] == "assistant")
            .map(text)
            .collect();
        assert_eq!(
            mine,
            [decl.calls.render("I read it."), decl.calls.render("done")],
            "{}: the model's own text, nothing else",
            decl.name
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_message_opens_with_a_time_head() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "map_no_time").await;
    save(
        &kernel,
        &agent,
        &[
            json!({"role": "user", "content": "with an offset", "attributes": {"ts": T1, "utc_offset": 7200}}),
            json!({"role": "assistant", "content": "ok", "attributes": {"ts": T1, "utc_offset": -18_000}}),
        ],
    )
    .await;
    // a row stored before offsets were: no `utc_offset`
    as_agent(
        agent.clone(),
        crate::memory::store_call(
            &agent,
            &kernel,
            json!({"type": "append", "rows": [
                {"id": kernel.mint_id(), "role": "user", "content": "without one", "attributes": {"ts": T1}},
                {"id": kernel.mint_id(), "role": "assistant", "content": "fine", "attributes": {"ts": T1}},
            ]}),
        ),
    )
    .await
    .expect("appended");
    crate::memory::forget_head(&agent, &kernel);
    let inflight = assembled(&kernel, &agent, &crate::row_attrs::Stamp::now()).await;
    for decl in REGISTRY {
        let sent = messages(decl, agent.as_str(), &inflight);
        assert_eq!(sent.len(), 6, "{sent:?}");
        for m in &sent {
            assert!(!opens_with_a_time(text(m)), "{}: {m}", decl.name);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thought_marker_sits_on_the_next_user_side_message() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "map_thought").await;
    let ids = save(
        &kernel,
        &agent,
        &[
            json!({"role": "user", "content": "u1"}),
            thinking("a1", 400),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a2"}),
        ],
    )
    .await;
    let inflight = assembled(&kernel, &agent, &crate::row_attrs::Stamp::now()).await;
    let u2 = &inflight[3];
    assert_eq!(
        pieces_of(u2),
        vec![
            Piece::Thought {
                tokens: 100,
                id: ids[1]
            },
            Piece::text("u2")
        ]
    );
    assert_eq!(pieces_of(&inflight[2]), vec![Piece::text("a1")]);
    // the arriving message carries none: the answer before it had no thinking
    assert_eq!(
        pieces_of(inflight.last().unwrap()),
        vec![Piece::text("next")]
    );
    for decl in REGISTRY {
        let sent = messages(decl, agent.as_str(), &inflight);
        let thought = row(
            decl,
            agent.as_str(),
            &[Piece::Thought {
                tokens: 100,
                id: ids[1],
            }],
        );
        assert_eq!(text(&sent[3]), format!("{thought}\n\nu2"), "{}", decl.name);
        assert_eq!(text(&sent[2]), "a1", "{}", decl.name);
    }

    // the newest answer thought: the arriving message is the next user-side one
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "map_thought_last").await;
    let ids = save(
        &kernel,
        &agent,
        &[json!({"role": "user", "content": "u1"}), thinking("a1", 8)],
    )
    .await;
    let inflight = assembled(&kernel, &agent, &crate::row_attrs::Stamp::now()).await;
    assert_eq!(
        pieces_of(inflight.last().unwrap()),
        vec![
            Piece::Thought {
                tokens: 2,
                id: ids[1]
            },
            Piece::text("next")
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_assistant_rows_in_a_row_stack_their_markers() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "map_stack").await;
    let ids = save(
        &kernel,
        &agent,
        &[
            json!({"role": "user", "content": "u1"}),
            thinking("a1", 40),
            thinking("a2", 80),
            json!({"role": "user", "content": "u2"}),
            json!({"role": "assistant", "content": "a3"}),
        ],
    )
    .await;
    let inflight = assembled(&kernel, &agent, &crate::row_attrs::Stamp::now()).await;
    assert_eq!(
        pieces_of(&inflight[4]),
        vec![
            Piece::Thought {
                tokens: 10,
                id: ids[1]
            },
            Piece::Thought {
                tokens: 20,
                id: ids[2]
            },
            Piece::text("u2"),
        ]
    );
}

/// One piece of every kind that stands for something not shown whole, for row `id`.
fn markers(id: i64) -> Vec<(&'static str, Piece)> {
    let tag = format!("[reply to {} · notes read]", id + 1);
    vec![
        (
            "folded_reply",
            Piece::Folded {
                kind: FoldKind::Reply,
                agent: "notes".into(),
                verb: "read".into(),
                tokens: 5003,
                id,
                tag_line: tag.clone(),
                head: "first \"line\" <of> it".into(),
                kept: None,
                what: String::new(),
            },
        ),
        (
            "folded_reply_kept",
            Piece::Folded {
                kind: FoldKind::Reply,
                agent: "notes".into(),
                verb: "read".into(),
                tokens: 5003,
                id,
                tag_line: tag.clone(),
                head: "first line".into(),
                kept: Some("the part I need".into()),
                what: String::new(),
            },
        ),
        (
            "folded_recall",
            Piece::Folded {
                kind: FoldKind::Recall,
                agent: "ai".into(),
                verb: "recall".into(),
                tokens: 5003,
                id,
                tag_line: tag.clone(),
                head: String::new(),
                kept: None,
                what: "query \"codename\"".into(),
            },
        ),
        (
            "folded_recall_kept",
            Piece::Folded {
                kind: FoldKind::Recall,
                agent: "ai".into(),
                verb: "recall".into(),
                tokens: 5003,
                id,
                tag_line: format!("[reply to {} · ai recall]", id + 1),
                head: String::new(),
                kept: Some("the gist".into()),
                what: "query \"codename\"".into(),
            },
        ),
        ("thought", Piece::Thought { tokens: 5003, id }),
        (
            "stub",
            Piece::Stub {
                tag_line: tag,
                bytes: 5003,
                id,
            },
        ),
        (
            "image_anchor",
            Piece::Image {
                form: ImageForm::Anchor,
                id: Some(id),
                name: "pic.png".into(),
                width: 64,
                height: 48,
                media_type: "image/png".into(),
                tokens: 5003,
            },
        ),
    ]
}

/// The unfold call a marker written by `decl` holds, read by the type's reader, as the
/// internal call text it gives on.
fn unfold_call(decl: &Decl<'_>, marker: &str) -> String {
    let call = if decl.name == DEFAULT {
        let at = marker
            .find("<call name=\"send\">")
            .expect("a call in the marker");
        let end = marker[at..].find("</call>").expect("a closed call") + at + "</call>".len();
        marker[at..end].to_string()
    } else {
        let line = marker
            .lines()
            .find(|l| l.starts_with('{'))
            .expect("a JSON line in the marker");
        let v: Value = serde_json::from_str(line).expect("the marker line is JSON");
        let mut unfold = String::new();
        super::hermes::write_json(&v["unfold"], &mut unfold);
        format!("<tool_call>\n{unfold}\n</tool_call>")
    };
    // the reader takes it with no `Bad`: what it gives on is the internal call text
    let mut o = decl.reader.outbound();
    let mut outs = o.feed(&call);
    outs.extend(o.finish());
    outs.into_iter()
        .map(|x| match x {
            Out::Text(t) | Out::Call { text: t } => t,
            Out::Bad { report } => panic!("{}: the unfold call is refused: {report}", decl.name),
            other => panic!("{}: {other:?} reading {call}", decl.name),
        })
        .collect()
}

#[test]
fn every_marker_names_its_id_and_the_recall_call() {
    let id = 4242;
    for decl in REGISTRY {
        for (kind, p) in markers(id) {
            let m = row(decl, "ai", std::slice::from_ref(&p));
            assert!(m.contains("4242"), "{} {kind}: no id: {m}", decl.name);
            assert!(m.contains("5003"), "{} {kind}: no size: {m}", decl.name);
            if kind.starts_with("folded_recall") {
                assert!(
                    m.contains("codename"),
                    "{} {kind}: what it recalled: {m}",
                    decl.name
                );
            }
            if let Piece::Folded { kept: Some(k), .. } = &p {
                assert!(
                    m.contains(k.as_str()),
                    "{} {kind}: the kept summary: {m}",
                    decl.name
                );
            }
            let call = unfold_call(decl, &m);
            let parsed = crate::tool_parse::extract_tool_calls(&call);
            assert_eq!(parsed.len(), 1, "{} {kind}: {call}", decl.name);
            let a = &parsed[0].arguments;
            assert_eq!(
                a["target_id"],
                json!("ai"),
                "{} {kind}: to the chat itself",
                decl.name
            );
            assert_eq!(
                a["payload"]["type"],
                json!("recall"),
                "{} {kind}",
                decl.name
            );
            assert_eq!(
                crate::tool_parse::typed::integer("id", &a["payload"]["id"]).ok(),
                Some(id),
                "{} {kind}: {call}",
                decl.name
            );
        }
        // the keep cue is the type's `keep` call for the block
        let cue = row(decl, "ai", &[Piece::KeepCue { pid: 4242 }]);
        assert!(
            cue.contains("4242") && cue.contains("keep"),
            "{}: {cue}",
            decl.name
        );
        // an image the model cannot be shown names it and says so: a recall brings it no
        // closer, so the marker holds no call
        let alt = row(
            decl,
            "ai",
            &[Piece::Image {
                form: ImageForm::Alt,
                id: None,
                name: "pic.png".into(),
                width: 64,
                height: 48,
                media_type: "image/png".into(),
                tokens: 4,
            }],
        );
        assert!(
            alt.contains("pic.png") && alt.contains("not shown to this model"),
            "{alt}"
        );
    }
}

#[test]
fn a_folded_reply_keeps_the_tag_line_and_the_kept_summary() {
    let tag = "[reply to 12 · notes read · \"the plan\"]";
    let kept = Piece::Folded {
        kind: FoldKind::Reply,
        agent: "notes".into(),
        verb: "read".into(),
        tokens: 900,
        id: 40,
        tag_line: tag.into(),
        head: "first line".into(),
        kept: Some("three steps; step 2 is the long one".into()),
        what: String::new(),
    };
    for decl in REGISTRY {
        let m = row(decl, "ai", std::slice::from_ref(&kept));
        assert!(m.starts_with(&format!("{tag}\n")), "{}: {m}", decl.name);
        assert!(
            m.contains("three steps; step 2 is the long one"),
            "{}: {m}",
            decl.name
        );
        // the block's marker is in the type's reply form
        if decl.name == super::hermes::NAME {
            assert!(m.contains("<tool_response>\n{\"folded\": true"), "{m}");
            assert!(m.ends_with("\n</tool_response>"), "{m}");
        } else {
            assert!(m.contains("\n<folded id=\"40\""), "{m}");
        }
    }
}

#[test]
fn map_is_pure() {
    let a = json!({"id": 7, "role": "user", "content": "[reply to 1 · notes read]\nbody",
                   "attributes": {"ts": T1, "utc_offset": 7200, "source": "lead"},
                   "pieces": [{"thought": {"tokens": 3, "id": 6}},
                              {"text": {"text": "[reply to 1 · notes read]\nbody"}}]});
    // the same message, every map's keys in another order
    let b: Value = serde_json::from_str(
        r#"{"pieces": [{"thought": {"id": 6, "tokens": 3}},
                       {"text": {"text": "[reply to 1 · notes read]\nbody"}}],
            "attributes": {"source": "lead", "utc_offset": 7200, "ts": 1795117200},
            "content": "[reply to 1 · notes read]\nbody", "role": "user", "id": 7}"#,
    )
    .unwrap();
    assert_eq!(canonical(&Shown::of(&a)), canonical(&Shown::of(&b)));
    for decl in REGISTRY {
        let once = message(decl, "ai", &Shown::of(&a));
        assert_eq!(
            message(decl, "ai", &Shown::of(&a)),
            once,
            "{}: twice",
            decl.name
        );
        assert_eq!(
            message(decl, "ai", &Shown::of(&b)),
            once,
            "{}: key order",
            decl.name
        );
    }
}

/// The environment section of a block mapped by the default type.
fn env_line(block: &str) -> &str {
    block
        .lines()
        .find(|l| l.starts_with("This thread began on "))
        .unwrap_or_else(|| panic!("no environment section: {block}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_env_block_reads_the_first_row_of_the_thread() {
    let decl = of(DEFAULT);
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "map_env").await;
    let block = |m: &[Value]| text(&message(decl, agent.as_str(), &Shown::of(&m[0]))).to_string();
    // an empty store: the arriving message is the thread's first row
    let first = assembled(&kernel, &agent, &stamp_at(T1)).await;
    assert!(env_line(&block(&first)).contains("2026-11-19 (UTC+00:00)"));
    // rows stored: the first row's date, not the arriving message's
    save(
        &kernel,
        &agent,
        &[
            json!({"role": "user", "content": "u1", "attributes": {"ts": T1, "utc_offset": 7200}}),
            json!({"role": "assistant", "content": "a1", "attributes": {"ts": T2}}),
        ],
    )
    .await;
    let later = assembled(&kernel, &agent, &stamp_at(T2 + 86_400)).await;
    assert!(
        env_line(&block(&later)).contains("2026-11-19 (UTC+02:00)"),
        "{}",
        block(&later)
    );
    // a clear starts a new thread: its row dates it
    save(
        &kernel,
        &agent,
        &[
            json!({"role": "user", "kind": "clear", "refs": [], "title": "cleared",
                 "content": "[cleared 9 · 2 rows] — a new thread",
                 "attributes": {"ts": T2, "utc_offset": 0}}),
        ],
    )
    .await;
    let cleared = assembled(&kernel, &agent, &stamp_at(T2 + 2 * 86_400)).await;
    assert!(
        env_line(&block(&cleared)).contains("2026-11-24 (UTC+00:00)"),
        "{}",
        block(&cleared)
    );
}

/// A system block of every section, with a memory store and a date.
fn every_section() -> Vec<SysPiece> {
    vec![
        SysPiece {
            part: "self_reflect".into(),
            body: SysBody::Text("You are `ai`.".into()),
        },
        SysPiece {
            part: String::new(),
            body: SysBody::Text("\n\n".into()),
        },
        SysPiece {
            part: "verb_docs".into(),
            body: SysBody::Howto(Howto {
                sections: vec![
                    Section::Env,
                    Section::Send,
                    Section::Reflect,
                    Section::Context,
                    Section::Memory,
                    Section::Titles,
                    Section::Async,
                    Section::LineSummary,
                    Section::Working,
                ],
                store: "mem".into(),
                env: Some(Env {
                    ts: T1,
                    offset: 7200,
                }),
            }),
        },
    ]
}

#[test]
fn no_default_call_form_in_a_hermes_system_block() {
    let block = system(of(super::hermes::NAME), &every_section());
    for form in ["<payload>", "<type>", "<call name="] {
        assert!(
            !block.contains(form),
            "{form} in the hermes block:\n{block}"
        );
    }
    // and the default block is in its own form
    let d = system(of(DEFAULT), &every_section());
    assert!(d.contains("<call name=") && !d.contains("<tool_call>"));
}

#[test]
fn every_section_and_marker_of_every_decl_is_a_resource_text() {
    for decl in REGISTRY {
        for s in Section::ALL {
            let t = (decl.texts)(s);
            assert!(!t.trim().is_empty(), "{} {s:?}", decl.name);
            if let Some(shared) = super::shared_text(s) {
                assert_eq!(t, shared, "{} {s:?}: one text for every type", decl.name);
            }
        }
        for (kind, t) in decl.markers.all() {
            assert!(!t.trim().is_empty(), "{} {kind}", decl.name);
        }
    }
    // a section that shows a call form is the type's own (the stop reason is a file per
    // type that shows none)
    let (d, h) = (of(DEFAULT), of(super::hermes::NAME));
    for s in Section::ALL
        .into_iter()
        .filter(|s| super::shared_text(*s).is_none() && *s != Section::StopReason)
    {
        assert_ne!((d.texts)(s), (h.texts)(s), "{s:?}");
    }
}

#[test]
fn a_hermes_view_with_a_context_notice_holds_no_default_call_form() {
    let notice =
        crate::projection::context_notice("strip", Some("the plan, step 2"), false, 4, 3, 900);
    let view = vec![
        json!({"role": "system", "sys": every_section()}),
        notice.clone(),
        json!({"role": "user", "content": "go"}),
    ];
    let sent = messages(of(super::hermes::NAME), "ai", &view);
    let n = text(&sent[1]);
    assert!(n.starts_with("[context-notice] "), "{n}");
    assert!(
        n.contains("the plan, step 2") && n.contains("3 reply bod(ies)"),
        "{n}"
    );
    assert!(n.contains("\"payload\": {\"type\": \"recall\"}"), "{n}");
    for m in &sent {
        for form in ["<payload>", "<type>", "<call name="] {
            assert!(!text(m).contains(form), "{form} in the hermes view: {m}");
        }
    }
    // the default type keeps its own call form for it
    let d = text(&message(of(DEFAULT), "ai", &Shown::of(&notice))).to_string();
    assert!(
        d.contains("`<type>recall</type>` (optional `<query>`, `<limit>`)"),
        "{d}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_row_with_no_ts_shows_no_date_never_the_clocks() {
    let tmp = TempDir::new().unwrap();
    let (kernel, agent) = mk_kernel(&tmp, "map_env_none").await;
    // an old row, appended as it was: no `ts`
    as_agent(
        agent.clone(),
        crate::memory::store_call(
            &agent,
            &kernel,
            json!({"type": "append", "rows": [
                {"id": kernel.mint_id(), "role": "user", "content": "old"},
            ]}),
        ),
    )
    .await
    .expect("appended");
    crate::memory::forget_head(&agent, &kernel);
    for at in [T1, T2] {
        let m = assembled(&kernel, &agent, &stamp_at(at)).await;
        let block = text(&message(of(DEFAULT), agent.as_str(), &Shown::of(&m[0]))).to_string();
        assert!(!block.contains("This thread began on "), "{block}");
    }
}
