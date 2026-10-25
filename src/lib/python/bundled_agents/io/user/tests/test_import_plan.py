"""The pure import plan: hand-written blueprints, no kernel (mirrors the Rust
import tests)."""

from __future__ import annotations

from user.import_ import Edge, ImportError, World, plan

ROOT = "kernel_root"


def existing(id, taken):
    """The agents that exist: memory (a store), exec, and `taken`."""
    if id in ("memory", "memory2"):
        return "sqlite_store.tools"
    if id == "exec":
        return "exec_bridge.tools"
    if id == "home":
        return "user.tools"
    if id == ROOT:
        return ""
    if id in taken:
        return "echo.tools"
    return None


def known(hm):
    return hm in ("echo.tools", "sqlite_store.tools", "exec_bridge.tools")


def run(yaml, files=(), bind=None, allow_code=False, taken=()):
    """Plan, raising ImportError on refusal."""
    w = World(
        parent_id="home",
        root_id=ROOT,
        bind=dict(bind or {}),
        allow_code=allow_code,
        agent=lambda id: existing(id, taken),
        known_bundle=known,
    )
    return plan(yaml, dict(files), w)


def refused(*a, **k) -> ImportError:
    try:
        run(*a, **k)
    except ImportError as e:
        return e
    raise AssertionError("expected a refusal")


def agent(p, id):
    return next(a for a in p.agents if a.new_id == id)


THREE = """
agents:
  - id: a
    handler_module: echo.tools
    can_call: {b: '*', $self: '*'}
    agents:
      - id: b
        handler_module: echo.tools
        callable_by: {a: '*', $parent: '*'}
        can_call: {'*': '*'}
        agents:
          - id: c
            handler_module: echo.tools
            notes: plain
"""


def test_plan_gives_every_agent_a_fresh_suffixed_id():
    p = run(THREE)
    assert list(p.ids.items()) == [("a", "a_1"), ("b", "b_1"), ("c", "c_1")]
    assert [(a.new_id, a.parent) for a in p.agents] == [
        ("a_1", "home"),
        ("b_1", "a_1"),
        ("c_1", "b_1"),
    ]
    assert agent(p, "c_1").meta["notes"] == "plain"
    assert p.edges == []


def test_plan_picks_next_free_suffix_when_taken():
    p = run(THREE, taken=["a_1"])
    assert list(p.ids.values()) == ["a_2", "b_2", "c_2"]
    # one taken anywhere moves them all
    p = run(THREE, taken=["c_1", "b_2"])
    assert list(p.ids.values()) == ["a_3", "b_3", "c_3"]


def test_plan_rewrites_inside_grants_on_both_ends():
    p = run(THREE)
    assert agent(p, "a_1").meta["can_call"] == {"b_1": "*", "$self": "*"}
    assert agent(p, "b_1").meta["callable_by"] == {"a_1": "*", "$parent": "*"}
    assert agent(p, "b_1").meta["can_call"] == {"*": "*"}


def test_plan_resolves_root_port():
    y = """
agents:
  - id: a
    handler_module: echo.tools
    can_call: {'@root': ['reflect']}
"""
    p = run(y)
    assert agent(p, "a_1").meta["can_call"] == {ROOT: ["reflect"]}
    # the root grants back, and the preview shows it
    assert p.edges == [Edge(ROOT, "callable_by", "a_1", ["reflect"])]
    assert p.reaches[0]["ports"]["@root"] == ROOT


PORTED = """
agents:
  - id: a
    handler_module: echo.tools
    can_call: {'@memory': '*'}
    callable_by: {'@exec': ['emit']}
    memory_store_id: '@memory'
    credentials: {store: '@memory', ref: r1}
ports:
  '@memory': {kind: memory, why: x, used_by: [a]}
  '@exec': {kind: exec, why: y, used_by: [a]}
"""


def test_plan_binds_ports_into_grants_and_refs():
    p = run(PORTED, bind={"@memory": "memory2", "@exec": "exec"})
    a = agent(p, "a_1")
    assert a.meta["can_call"] == {"memory2": "*"}
    assert a.meta["callable_by"] == {"exec": ["emit"]}
    assert a.meta["memory_store_id"] == "memory2"
    assert a.meta["credentials"] == {"store": "memory2", "ref": "r1"}
    assert p.edges == [
        Edge("memory2", "callable_by", "a_1", "*"),
        Edge("exec", "can_call", "a_1", ["emit"]),
    ]
    assert p.ports["@memory"] == "memory2"
    assert p.reaches[0]["can_call"] == ["memory2"]
    assert p.reaches[0]["callable_by"] == ["exec"]


def test_plan_refuses_unbound_ports_naming_all():
    e = refused(PORTED)
    assert e.reason == "unbound_ports"
    for want in ["@memory (memory)", "@exec (exec)", "pass bind {", '"<agent id>"']:
        assert want in e.message, f"{want}: {e.message}"
    # one bound: the other is still named
    e = refused(PORTED, bind={"@memory": "memory"})
    assert "@exec" in e.message and "@memory (" not in e.message, e.message


def test_plan_refuses_bind_of_wrong_kind():
    e = refused(PORTED, bind={"@memory": "exec", "@exec": "exec"})
    assert e.reason == "bad_binding"
    for want in ["@memory", "wants a memory agent", "exec (exec_bridge.tools)"]:
        assert want in e.message, f"{want}: {e.message}"
    # an agent that does not exist
    e = refused(PORTED, bind={"@memory": "nope", "@exec": "exec"})
    assert e.reason == "bad_binding", e.message


def test_plan_refuses_bind_of_missing_agent_naming_the_port():
    e = refused(PORTED, bind={"@memory": "nope", "@exec": "exec"})
    assert e.reason == "bad_binding"
    for want in ["@memory", "nope", "no agent"]:
        assert want in e.message, f"{want}: {e.message}"


def test_plan_refuses_bind_key_that_is_not_a_port():
    e = refused(
        PORTED, bind={"@memory": "memory", "@exec": "exec", "@model": "memory"}
    )
    assert e.reason == "bad_binding"
    assert "@model" in e.message, e.message


def test_plan_refuses_path_port_for_now():
    y = """
agents:
  - id: a
    handler_module: echo.tools
    cwd: '@path:cwd'
ports:
  '@path:cwd': {kind: path, why: x, used_by: [a]}
"""
    e = refused(y, bind={"@path:cwd": "exec"})
    assert e.reason == "unbound_ports"
    assert "not bindable yet" in e.message and "@path:cwd" in e.message, e.message


def test_plan_refuses_a_key_value_naming_agent_and_key_not_value():
    planted = "sk-planted0123456789"
    cases = [
        (
            f"agents:\n  - id: a\n    handler_module: echo.tools\n    api_key: {planted}\n",
            [],
            "a api_key",
        ),
        (
            f"agents:\n  - id: a\n    handler_module: echo.tools\n    llm: {{opts: {{token: {planted}}}}}\n",
            [],
            "a token",
        ),
        (
            "agents:\n  - id: a\n    handler_module: echo.tools\n    rows: [{text: hi}]\n",
            [],
            "a rows",
        ),
        (
            "agents:\n  - id: a\n    handler_module: echo.tools\n    system_prompt_file: a/directive.md\n",
            [("a/directive.md", "use sk-planted0123456789 please")],
            "a/directive.md",
        ),
    ]
    for y, files, want in cases:
        e = refused(y, files)
        assert e.reason == "secret_found", y
        ident, _, key = want.partition(" ")
        assert ident in e.message and key in e.message, f"{want}: {e.message}"
        assert planted not in e.message, e.message
        assert "export again" in e.message, e.message
    # the message names the agent and the key
    e = refused(cases[0][0])
    assert "agent a: a api_key key" in e.message, e.message
    # a reference to a key is not a key
    y = "agents:\n  - id: a\n    handler_module: echo.tools\n    credentials: {ref: r1}\n"
    run(y)


def test_plan_refuses_absolute_path():
    y = "agents:\n  - id: a\n    handler_module: echo.tools\n    cwd: /private/x\n"
    e = refused(y)
    assert e.reason == "secret_found"
    assert "agent a field cwd: an absolute path" in e.message, e.message
    assert "/private" not in e.message, e.message


def test_plan_refuses_code_without_allow_code_and_accepts_with_it():
    y = """
agents:
  - id: a
    handler_module: echo.tools
    system_prompt_file: a/directive.md
    agents:
      - id: s
        handler_module: py_ab12.tools
"""
    files = [("a/directive.md", "be kind"), ("s/bundle.yaml", "name: s")]
    e = refused(y, files)
    assert e.reason == "code_not_allowed"
    for want in ["agent s", "py_ab12.tools", "file s/bundle.yaml", "allow_code: true"]:
        assert want in e.message, f"{want}: {e.message}"
    assert "directive.md" not in e.message, e.message
    p = run(y, files, allow_code=True)
    assert len(p.code) == 2, p.code
    # a directive alone is not code
    y = "agents:\n  - id: a\n    handler_module: echo.tools\n"
    assert run(y, [("a/directive.md", "hi")]).code == []


def test_plan_refuses_parent_id_in_entry():
    y = "agents:\n  - id: a\n    handler_module: echo.tools\n    parent_id: home\n"
    e = refused(y)
    assert e.reason == "bad_arg"
    assert "parent_id" in e.message, e.message
    assert e.message.startswith("user.import_subtree: ")


def test_plan_refuses_two_top_agents_or_missing_agents_key():
    for y in [
        "agents:\n  - {id: a, handler_module: echo.tools}\n  - {id: b, handler_module: echo.tools}\n",
        "ports: {}\n",
        "agents: []\n",
        "not: [yaml",
    ]:
        e = refused(y)
        assert e.reason == "bad_arg", f"{y}: {e.message}"


def test_plan_refuses_undeclared_port_and_unknown_outside_id():
    y = "agents:\n  - id: a\n    handler_module: echo.tools\n    can_call: {'@model': '*'}\n"
    e = refused(y)
    assert e.reason == "bad_arg" and "@model" in e.message, e.message
    # a literal outside id is neither the blueprint's nor a port
    y = "agents:\n  - id: a\n    handler_module: echo.tools\n    can_call: {memory: '*'}\n"
    e = refused(y)
    assert e.reason == "bad_arg" and "memory" in e.message, e.message
