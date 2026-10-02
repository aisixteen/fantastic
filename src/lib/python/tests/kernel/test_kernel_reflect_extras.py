"""Uniform `reflect` surface — the composable flags (tree / bundles /
readme) and the `description` field.
"""

from __future__ import annotations


async def test_reflect_root_uniform(seeded_kernel):
    """Root reflect is the uniform identity."""
    r = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert r["id"] == "kernel_state"
    assert r["sentence"].startswith("Fantastic kernel")
    assert r["parent_id"] is None


async def test_reflect_root_has_runtime(seeded_kernel):
    """Root reflect carries the kernel `runtime` id + deployment context
    (`env`, `version`) so a client that hops to this kernel can gate behavior
    from one round-trip. Non-root agents omit all three."""
    r = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert r["runtime"] == "python"
    # No FANTASTIC_ENV / FANTASTIC_VERSION in a bare test process → host defaults.
    assert r["env"] == "host"
    assert "version" in r and r["version"] is None
    child = (
        await seeded_kernel.send([{"target": "cli", "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert "runtime" not in child
    assert "env" not in child and "version" not in child


async def test_reflect_root_env_version_from_environ(seeded_kernel, monkeypatch):
    """`env`/`version` are read at reflect time from the optional FANTASTIC_ENV /
    FANTASTIC_VERSION envs the container bakes in — so a client learns it is
    talking to a disposable container and which build, in one round-trip."""
    monkeypatch.setenv("FANTASTIC_ENV", "container")
    monkeypatch.setenv("FANTASTIC_VERSION", "v9.9.9")
    r = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert r["env"] == "container"
    assert r["version"] == "v9.9.9"


async def test_kernel_address_equals_the_root(seeded_kernel):
    """`kernel` is the reserved address of the root; reflecting it == reflecting kernel_state."""
    via_address = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    via_id = (
        await seeded_kernel.send(
            [{"target": "kernel_state", "payload": {"type": "reflect"}}]
        )
    )[0]["result"]
    assert via_address == via_id


# ─── tree tiers ────────────────────────────────────────────────


async def test_reflect_tree_all_default(seeded_kernel):
    """Default tree=all: nested distilled subtree rooted at the agent."""
    r = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    tree = r["tree"]
    assert tree["id"] == "kernel_state"
    child_ids = {c["id"] for c in tree.get("children", [])}
    assert "cli" in child_ids


async def test_reflect_tree_node_carries_a_bridges_allow(seeded_kernel):
    """`allow` is posture (audit 2026-09-28, d7): what a file_bridge exposes is
    readable from the ONE root reflect (`detail:full`), like its root and its lock."""
    seeded_kernel.create("file_bridge.tools", allow=["src", "README.md"], id="fb_d7")
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "detail": "full"}}]
        )
    )[0]["result"]
    node = next(c for c in r["tree"]["children"] if c["id"] == "fb_d7")
    assert node.get("allow") == ["src", "README.md"], node


async def test_reflect_tree_ids(seeded_kernel):
    """tree=ids: a flat descendant-id list (self first)."""
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "tree": "ids"}}]
        )
    )[0]["result"]
    assert isinstance(r["tree"], list)
    assert r["tree"][0] == "kernel_state"
    assert "cli" in r["tree"]


async def test_reflect_tree_none_omits(seeded_kernel):
    """tree=none: no tree key at all."""
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "tree": "none"}}]
        )
    )[0]["result"]
    assert "tree" not in r


# ─── bundles tiers ─────────────────────────────────────────────


async def test_reflect_bundles_default_omitted(seeded_kernel):
    r = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert "bundles" not in r


async def test_reflect_bundles_all(seeded_kernel):
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "bundles": "all"}}]
        )
    )[0]["result"]
    bundles = r["bundles"]
    assert isinstance(bundles, list)
    names = [b["name"] for b in bundles]
    assert "cli" in names and "file_bridge" in names
    assert names == sorted(names)
    for b in bundles:
        # `schema` only where the bundle publishes a RECORD_SCHEMA (lib request #3)
        assert set(b.keys()) <= {"name", "handler_module", "schema"}
        assert b["handler_module"].endswith(".tools")


async def test_the_catalog_carries_each_backend_kinds_record_schema(seeded_kernel):
    """A set-up page reads what a KIND's record takes before any agent of it
    exists: every field with required/type/enum/default/hint — a key included,
    as a `credentials` field (a store ref), never an environment variable."""
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "bundles": "all"}}]
        )
    )[0]["result"]
    by = {b["name"]: b for b in r["bundles"]}
    vllm = by["vllm_backend"]["schema"]
    fields = {f["name"]: f for f in vllm["fields"]}
    required = [f["name"] for f in vllm["fields"] if f["required"]]
    assert required == [
        "endpoint",
        "model",
        "max_tokens",
        "kv_slots",
        "kv_overflow",
        "memory_store_id",
    ], required
    # a required field has no default — nothing is guessed about a server
    assert all("default" not in fields[n] for n in required)
    # the defaulted fields carry THE BACKEND'S OWN values
    assert fields["temperature"]["default"] == 0.2
    assert fields["thinking_token_budget"]["default"] == 16000
    assert fields["kv_overflow"]["enum"] == ["evict", "reject"]
    assert fields["reasoning_effort"]["enum"] == ["low", "medium", "xhigh"]
    # the key: optional where a server may be keyless, required where it never is
    assert fields["credentials"]["type"] == "credentials"
    assert fields["credentials"]["required"] is False
    sch = by["responses_backend"]["schema"]
    assert "key_var" not in sch
    cred = next(f for f in sch["fields"] if f["name"] == "credentials")
    assert cred["required"] is True
    # a bundle that is not a backend publishes none
    assert "schema" not in by["file_bridge"]


async def test_the_schema_is_what_the_write_enforces(seeded_kernel):
    """ONE TRUTH: a create missing the schema's required fields is refused at the
    write, every one named; an enum value outside its set is refused too."""
    k = seeded_kernel

    async def create(**meta):
        return (
            await k.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": k.id,
                            "handler_module": "responses_backend.tools",
                            **meta,
                        },
                    }
                ]
            )
        )[0]["result"]

    r = await create(id="a1")
    assert r.get("reason") == "invalid_meta", r
    assert "missing: endpoint, model, credentials, memory_store_id" in r["error"], r
    r = await create(
        id="a2",
        endpoint="http://127.0.0.1:9/stub",
        model="m",
        memory_store_id="memory",
        credentials={"store": "creds", "ref": "main"},
        context_strategy="bogus",
    )
    assert r.get("reason") == "invalid_meta" and "context_strategy" in r["error"], r
    r = await create(
        id="a3",
        endpoint="http://127.0.0.1:9/stub",
        model="m",
        memory_store_id="memory",
        credentials={"store": "creds"},
    )
    assert r.get("reason") == "invalid_meta", r
    assert "credentials must be {store" in r["error"], r
    r = await create(
        id="a4",
        endpoint="http://127.0.0.1:9/stub",
        model="m",
        memory_store_id="memory",
        credentials={"store": "creds", "ref": "main"},
    )
    assert r.get("id") == "a4", r


async def test_reflect_bundles_ids(seeded_kernel):
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "bundles": "ids"}}]
        )
    )[0]["result"]
    assert isinstance(r["bundles"], list)
    assert all(isinstance(n, str) for n in r["bundles"])
    assert "file_bridge" in r["bundles"]
    assert r["bundles"] == sorted(r["bundles"])


# ─── readme tier ───────────────────────────────────────────────


async def test_reflect_readme_default_omitted(seeded_kernel):
    r = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert "readme" not in r


async def test_reflect_readme_flag(seeded_kernel):
    """readme=true attaches the root readme."""
    new = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "readme": True}}]
        )
    )[0]["result"]
    assert isinstance(new["readme"], str)
    assert "Fantastic kernel" in new["readme"]


# ─── description field ─────────────────────────────────────────


async def test_reflect_description_surfaces_top_and_tree(seeded_kernel):
    """description set at create surfaces both at top-level reflect of the
    agent AND in its node inside a tree=all walk of the parent."""
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "description": "holds my notes",
                    },
                }
            ]
        )
    )[0]["result"]
    fid = rec["id"]
    own = (await seeded_kernel.send([{"target": fid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert own["description"] == "holds my notes"
    root = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    node = next(c for c in root["tree"]["children"] if c["id"] == fid)
    assert node["description"] == "holds my notes"
