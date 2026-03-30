"""Tests for tools — registry and _format_outputs helper (unit-level)."""

import time


from core.engine import Engine
from core.tools import (
    _format_outputs,
)


# ─── Engine: Server registry ──────────────────────────────────────────


class TestServerRegistry:
    def setup_method(self):
        import tempfile

        self._tmp = tempfile.mkdtemp()
        self.engine = Engine(project_dir=self._tmp)

    def test_register_and_list(self):
        entry = self.engine.register_server(
            "a1", "http://localhost:9001/api", name="my-tools"
        )
        assert entry["agent_id"] == "a1"
        assert entry["url"] == "http://localhost:9001/api"
        assert entry["name"] == "my-tools"

        servers = self.engine.list_servers()
        assert len(servers) == 1
        assert servers[0]["agent_id"] == "a1"

    def test_register_default_name(self):
        entry = self.engine.register_server("a1", "http://localhost:9001/api")
        assert entry["name"] == "agent-a1"

    def test_register_with_tools(self):
        entry = self.engine.register_server(
            "a1", "http://localhost:9001", tools=["analyze", "summarize"]
        )
        assert entry["tools"] == ["analyze", "summarize"]

    def test_unregister(self):
        self.engine.register_server("a1", "http://localhost:9001")
        assert self.engine.unregister_server("a1") is True
        assert self.engine.list_servers() == []

    def test_unregister_nonexistent(self):
        assert self.engine.unregister_server("a1") is False

    def test_get_server(self):
        self.engine.register_server("a1", "http://localhost:9001")
        server = self.engine.get_server("a1")
        assert server is not None
        assert server["agent_id"] == "a1"

    def test_get_server_nonexistent(self):
        assert self.engine.get_server("a1") is None

    def test_re_register_overwrites(self):
        self.engine.register_server("a1", "http://localhost:9001", name="old")
        self.engine.register_server("a1", "http://localhost:9002", name="new")
        servers = self.engine.list_servers()
        assert len(servers) == 1
        assert servers[0]["url"] == "http://localhost:9002"
        assert servers[0]["name"] == "new"

    def test_multiple_servers(self):
        self.engine.register_server("a1", "http://localhost:9001")
        self.engine.register_server("a2", "http://localhost:9002")
        self.engine.register_server("a3", "http://localhost:9003")
        assert len(self.engine.list_servers()) == 3

    def test_registered_at_timestamp(self):
        before = time.time()
        entry = self.engine.register_server("a1", "http://localhost:9001")
        after = time.time()
        assert before <= entry["registered_at"] <= after

    def test_registry_persisted(self):
        """Registry survives engine restart."""
        self.engine.register_server("a1", "http://localhost:9001", name="test")
        engine2 = Engine(project_dir=self._tmp)
        servers = engine2.list_servers()
        assert len(servers) == 1
        assert servers[0]["name"] == "test"


# ─── _format_outputs helper ───────────────────────────────────────────────


class TestFormatOutputs:
    def test_stream(self):
        out = _format_outputs([{"output_type": "stream", "text": "hello\n"}])
        assert out == "hello\n"

    def test_error(self):
        out = _format_outputs(
            [
                {
                    "output_type": "error",
                    "ename": "ValueError",
                    "evalue": "bad",
                    "traceback": ["Traceback...", "ValueError: bad"],
                }
            ]
        )
        assert "ValueError: bad" in out

    def test_execute_result(self):
        out = _format_outputs(
            [
                {
                    "output_type": "execute_result",
                    "data": {"text/plain": "42"},
                    "metadata": {},
                }
            ]
        )
        assert out == "42"

    def test_image_placeholder(self):
        out = _format_outputs(
            [
                {
                    "output_type": "display_data",
                    "data": {"image/png": "base64data"},
                    "metadata": {},
                }
            ]
        )
        assert out == "[image/png output]"

    def test_empty_outputs(self):
        assert _format_outputs([]) == ""

    def test_multiple_outputs(self):
        out = _format_outputs(
            [
                {"output_type": "stream", "text": "line1\n"},
                {
                    "output_type": "execute_result",
                    "data": {"text/plain": "42"},
                    "metadata": {},
                },
            ]
        )
        assert "line1" in out
        assert "42" in out
