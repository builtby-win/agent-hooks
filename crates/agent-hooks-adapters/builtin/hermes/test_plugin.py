"""Behaviour tests for the staged Hermes plugin asset.

Run: python3 crates/agent-hooks-adapters/builtin/hermes/test_plugin.py
"""

import importlib.util
import json
import os
import stat
import tempfile
import time
import unittest

ASSET = os.path.join(os.path.dirname(__file__), "assets", "__init__.py")


def load(stage=None, env=None):
    """Load a private copy, substituting install tokens like the Rust stager."""
    src = open(ASSET).read()
    if stage is not None:
        for token, value in stage.items():
            src = src.replace('"%s"' % token, json.dumps(value))
    tmp = tempfile.NamedTemporaryFile("w", suffix=".py", delete=False)
    tmp.write(src)
    tmp.close()
    old = dict(os.environ)
    os.environ.update(env or {})
    try:
        spec = importlib.util.spec_from_file_location("ah_hermes_%d" % id(tmp), tmp.name)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod
    finally:
        os.environ.clear()
        os.environ.update(old)


class FakeBinary:
    def __init__(self, verdict=None):
        self.dir = tempfile.mkdtemp()
        self.log = os.path.join(self.dir, "calls.log")
        self.path = os.path.join(self.dir, "agent-hooks")
        verdict_line = "printf '%%s' '%s'" % json.dumps(verdict) if verdict else "true"
        with open(self.path, "w") as f:
            f.write(
                '#!/bin/sh\ninput=$(cat)\nprintf \'%%s\\t%%s\\n\' "$*" "$input" >> "%s"\n%s\n'
                % (self.log, verdict_line)
            )
        os.chmod(self.path, os.stat(self.path).st_mode | stat.S_IEXEC)

    def calls(self, expected=1, timeout=3.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if os.path.exists(self.log):
                lines = [l for l in open(self.log).read().splitlines() if l]
                if len(lines) >= expected:
                    return [(a, json.loads(p)) for a, p in (l.split("\t", 1) for l in lines)]
            time.sleep(0.02)
        return []


class HermesPluginTest(unittest.TestCase):
    def test_install_placeholders(self):
        mod = load()
        self.assertEqual(mod.hook_args("on_session_end")[1:], ["hook", "hermes", "--event", "on_session_end"])
        staged = load(
            {
                "__AGENT_HOOKS_BIN__": "/opt/agent-hooks",
                "__AGENT_HOOKS_SOCKET__": "/tmp/ah.sock",
                "__AGENT_HOOKS_CONTEXT_CMD__": "/opt/b2v",
            }
        )
        self.assertEqual(
            staged.hook_args("pre_llm_call"),
            ["/opt/agent-hooks", "hook", "hermes", "--event", "pre_llm_call", "--socket", "/tmp/ah.sock", "--context-cmd", "/opt/b2v"],
        )

    def test_kill_switch(self):
        mod = load()
        self.assertFalse(mod.hooks_disabled({}))
        self.assertTrue(mod.hooks_disabled({"AGENT_HOOKS_DISABLED": "1"}))
        self.assertTrue(mod.hooks_disabled({"B2V_DISABLED": "true"}))
        self.assertFalse(mod.hooks_disabled({"B2V_DISABLED": "0"}))

    def test_payload_drops_large_objects(self):
        mod = load()
        p = mod.build_payload(
            "pre_llm_call",
            {"session_id": "h1", "conversation_history": [{"role": "user"}] * 1000, "user_message": "x" * 10000, "is_first_turn": True},
        )
        self.assertEqual(p, {"event": "pre_llm_call", "session_id": "h1", "is_first_turn": True})

    def test_subagent_events_belong_to_parent(self):
        mod = load()
        p = mod.build_payload("subagent_stop", {"parent_session_id": "root", "child_role": "explorer", "child_summary": "found it"})
        self.assertEqual(p["session_id"], "root")
        self.assertNotIn("parent_session_id", p)
        self.assertEqual(p["subagent_id"], "explorer")
        self.assertEqual(p["summary"], "found it")

    def test_tool_call_action(self):
        mod = load()
        self.assertIsNone(mod.tool_call_action({}))
        self.assertIsNone(mod.tool_call_action({"decision": "allow"}))
        self.assertEqual(mod.tool_call_action({"decision": "deny", "reason": "no"}), {"action": "block", "message": "no"})

    def test_end_to_end_register_and_forward(self):
        fake = FakeBinary(verdict={"decision": "deny", "reason": "policy"})
        mod = load({"__AGENT_HOOKS_BIN__": fake.path})
        hooks = {}

        class Ctx:
            def register_hook(self, name, fn):
                hooks[name] = fn

        mod.register(Ctx())
        # legacy b2v_hooks.py registered these five; the rest are new.
        for name in ("on_session_start", "pre_llm_call", "pre_tool_call", "pre_approval_request", "on_session_end"):
            self.assertIn(name, hooks)
        self.assertEqual(
            hooks["pre_tool_call"](session_id="h1", tool_name="terminal", args={"command": "rm -rf /"}),
            {"action": "block", "message": "policy"},
        )
        hooks["on_session_end"](session_id="h1", completed=True, interrupted=False)
        calls = fake.calls(2)
        self.assertEqual([a.split()[3] for a, _ in calls], ["pre_tool_call", "on_session_end"])
        self.assertEqual(calls[0][1]["tool_input"], {"command": "rm -rf /"})
        self.assertEqual(calls[1][1]["completed"], True)

    def test_disabled_never_spawns(self):
        fake = FakeBinary()
        mod = load({"__AGENT_HOOKS_BIN__": fake.path})
        os.environ["AGENT_HOOKS_DISABLED"] = "1"
        try:
            self.assertIsNone(mod.pre_tool_call(session_id="h1", tool_name="x"))
            mod._observer("pre_llm_call")(session_id="h1")
        finally:
            del os.environ["AGENT_HOOKS_DISABLED"]
        self.assertEqual(fake.calls(1, timeout=0.3), [])


if __name__ == "__main__":
    unittest.main()
