"""Tests for core.input_loop — command parsing via CoreRecipient."""

from core.recipients import CoreRecipient


def test_parse_add():
    r = CoreRecipient()
    result = r.parse("add bundle_a")
    assert result == (
        "add_bundle",
        {"bundle_name": "bundle_a", "name": "", "working_dir": "", "from_source": ""},
    )


def test_parse_add_with_name():
    r = CoreRecipient()
    result = r.parse("add bundle_a --name debug")
    assert result == (
        "add_bundle",
        {
            "bundle_name": "bundle_a",
            "name": "debug",
            "working_dir": "",
            "from_source": "",
        },
    )


def test_parse_remove():
    r = CoreRecipient()
    result = r.parse("remove bundle_b")
    assert result == ("remove_bundle", {"bundle_name": "bundle_b", "name": ""})


def test_parse_remove_with_name():
    r = CoreRecipient()
    result = r.parse("remove bundle_a --name debug")
    assert result == ("remove_bundle", {"bundle_name": "bundle_a", "name": "debug"})


def test_parse_list():
    r = CoreRecipient()
    result = r.parse("list")
    assert result == ("list_bundles", {})


def test_parse_log():
    r = CoreRecipient()
    result = r.parse("log")
    assert result == ("conversation_log", {"max_lines": 100})


def test_parse_log_with_count():
    r = CoreRecipient()
    result = r.parse("log 50")
    assert result == ("conversation_log", {"max_lines": 50})


def test_parse_add_with_working_dir():
    r = CoreRecipient()
    result = r.parse("add bundle_a --working-dir ./notebooks")
    assert result == (
        "add_bundle",
        {
            "bundle_name": "bundle_a",
            "name": "",
            "working_dir": "./notebooks",
            "from_source": "",
        },
    )


def test_parse_add_with_name_and_working_dir():
    r = CoreRecipient()
    result = r.parse("add bundle_a --name main --working-dir ./notebooks")
    assert result == (
        "add_bundle",
        {
            "bundle_name": "bundle_a",
            "name": "main",
            "working_dir": "./notebooks",
            "from_source": "",
        },
    )


def test_parse_add_with_from():
    r = CoreRecipient()
    result = r.parse("add vscode --from https://github.com/user/fantastic-vscode.git")
    assert result == (
        "add_bundle",
        {
            "bundle_name": "vscode",
            "name": "",
            "working_dir": "",
            "from_source": "https://github.com/user/fantastic-vscode.git",
        },
    )


def test_parse_add_with_from_local():
    r = CoreRecipient()
    result = r.parse("add myplugin --from /path/to/plugin")
    assert result == (
        "add_bundle",
        {
            "bundle_name": "myplugin",
            "name": "",
            "working_dir": "",
            "from_source": "/path/to/plugin",
        },
    )


def test_parse_run():
    r = CoreRecipient()
    assert r.parse("run quickstart") == ("run_bundle", {"bundle_name": "quickstart"})


def test_parse_run_no_arg():
    r = CoreRecipient()
    assert r.parse("run") is None


def test_parse_unknown_returns_none():
    r = CoreRecipient()
    assert r.parse("hello world") is None
    assert r.parse("some random text") is None
