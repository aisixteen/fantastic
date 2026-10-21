"""Self-test of the plug's model-per-role rule — no model, no network.

    cd e2e && python3 -m unittest backends.selftest -v
"""

from __future__ import annotations

import os
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from backends import backend  # noqa: E402

_STRIP = ("FANTASTIC_AI_MODEL", "ANTHROPIC_CACHING", "ANTHROPIC_ENDPOINT", "BACKEND")


def env(**extra: str):
    """The process env with every variable the plug reads removed, then `extra`."""
    clean = {k: v for k, v in os.environ.items() if not k.startswith(_STRIP)}
    clean.update(extra)
    return mock.patch.dict(os.environ, clean, clear=True)


class ModelPerRole(unittest.TestCase):
    def test_default_only(self):
        with env(FANTASTIC_AI_MODEL="haiku"):
            bk = backend("anthropic")
            self.assertEqual(bk.model_for("coder"), "haiku")
            self.assertEqual(bk.model_for("reviewer"), "haiku")
            self.assertEqual(bk.model_for(None), "haiku")
            self.assertEqual(bk.record()["model"], "haiku")
            self.assertEqual(bk.record(role="coder")["model"], "haiku")

    def test_one_role_overridden(self):
        with env(FANTASTIC_AI_MODEL="haiku", FANTASTIC_AI_MODEL_REVIEWER="sonnet"):
            bk = backend("anthropic")
            self.assertEqual(bk.model_for("reviewer"), "sonnet")
            self.assertEqual(bk.model_for("coder"), "haiku")
            self.assertEqual(bk.record(role="reviewer")["model"], "sonnet")
            self.assertEqual(bk.record(role="coder")["model"], "haiku")
            self.assertEqual(bk.record()["model"], "haiku")

    def test_two_roles_overridden_differently(self):
        with env(FANTASTIC_AI_MODEL="haiku", FANTASTIC_AI_MODEL_MASTER="opus",
                 FANTASTIC_AI_MODEL_REVIEWER="sonnet"):
            bk = backend("anthropic")
            self.assertEqual(bk.model_for("master"), "opus")
            self.assertEqual(bk.model_for("reviewer"), "sonnet")
            self.assertEqual(bk.model_for("worker"), "haiku")

    def test_unknown_role_gets_the_default(self):
        with env(FANTASTIC_AI_MODEL="haiku", FANTASTIC_AI_MODEL_REVIEWER="sonnet"):
            bk = backend("anthropic")
            self.assertEqual(bk.model_for("nobody-has-heard-of-this"), "haiku")
            self.assertEqual(bk.record(role="surveyor")["model"], "haiku")

    def test_empty_override_is_the_default(self):
        with env(FANTASTIC_AI_MODEL="haiku", FANTASTIC_AI_MODEL_REVIEWER=""):
            self.assertEqual(backend("anthropic").model_for("reviewer"), "haiku")

    def test_role_name_is_upper_cased_and_made_safe(self):
        with env(FANTASTIC_AI_MODEL="haiku", FANTASTIC_AI_MODEL_CODE_REVIEWER="sonnet"):
            bk = backend("anthropic")
            self.assertEqual(bk.model_for("code-reviewer"), "sonnet")
            self.assertEqual(bk.model_for("Code_Reviewer"), "sonnet")

    def test_record_for_a_role_keeps_every_other_field(self):
        with env(FANTASTIC_AI_MODEL="haiku", FANTASTIC_AI_MODEL_REVIEWER="sonnet",
                 ANTHROPIC_ENDPOINT="http://x/v1/messages", ANTHROPIC_CACHING="cleanup"):
            bk = backend("anthropic")
            plain, rev = bk.record(), bk.record(role="reviewer")
            self.assertEqual(rev["model"], "sonnet")
            self.assertEqual({k: v for k, v in rev.items() if k != "model"},
                             {k: v for k, v in plain.items() if k != "model"})
            self.assertEqual(rev["caching"], "cleanup")
            self.assertEqual(rev["endpoint"], "http://x/v1/messages")
            self.assertEqual(rev["handler_module"], "anthropic_backend.tools")
            self.assertIn("credentials", rev)

    def test_overrides_still_win_and_extra_fields_pass_through(self):
        with env(FANTASTIC_AI_MODEL="haiku", FANTASTIC_AI_MODEL_REVIEWER="sonnet"):
            bk = backend("anthropic")
            self.assertEqual(bk.record(role="reviewer", model="pinned")["model"], "pinned")
            self.assertEqual(bk.record(context_window=123)["context_window"], 123)

    def test_roles_used_lists_what_was_asked(self):
        with env(FANTASTIC_AI_MODEL="haiku", FANTASTIC_AI_MODEL_REVIEWER="sonnet"):
            bk = backend("anthropic")
            self.assertEqual(bk.roles_used(), {})
            bk.model_for("coder")
            bk.record(role="reviewer")
            bk.record()  # no role: not a role
            self.assertEqual(bk.roles_used(), {"coder": "haiku", "reviewer": "sonnet"})

    def test_the_plugs_refusal_for_a_missing_default_stays(self):
        with env(FANTASTIC_AI_MODEL_REVIEWER="sonnet"):
            with self.assertRaises(SystemExit) as cm:
                backend("anthropic")
            self.assertIn("FANTASTIC_AI_MODEL is unset", str(cm.exception))

    def test_foundation_models_names_no_model_for_any_role(self):
        with env(FANTASTIC_AI_MODEL_REVIEWER="sonnet"):
            bk = backend("foundation_models")
            self.assertEqual(bk.model_for("reviewer"), "")
            self.assertNotIn("model", bk.record(role="reviewer"))


if __name__ == "__main__":
    unittest.main()
