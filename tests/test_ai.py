"""The AI-app link (MCP server and its tools) and the local AI's answer checks, with no real AI app or model."""
import io
import json
import shutil
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from ai_link import mcp, tools
from crash_doctor import doctor
from local_ai import llm
from shared.store import Store
from tests.helpers import mod


def rpc(i, method, params=None):
    return {"jsonrpc": "2.0", "id": i, "method": method, **({"params": params} if params else {})}


class Mcp(unittest.TestCase):
    def test_handshake_list_and_calls(self):
        def call(name, args):
            if name == "bad":
                raise RuntimeError("window gone")
            return f"{name} {args}"
        msgs = [rpc(1, "initialize", {"protocolVersion": "2025-03-26"}), {"jsonrpc": "2.0", "method": "notifications/initialized"},
                "", "not json", rpc(2, "tools/list"), rpc(3, "tools/call", {"name": "status"}),
                rpc(4, "tools/call", {"name": "bad", "arguments": {}}), rpc(5, "nope"),
                {"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": [1]}, rpc(7, "ping")]
        out = io.BytesIO()
        mcp.serve(call, "9.9", io.BytesIO("".join((m if isinstance(m, str) else json.dumps(m)) + "\n" for m in msgs).encode()), out)
        r = [json.loads(x) for x in out.getvalue().splitlines()]
        self.assertEqual([x["id"] for x in r], [1, None, 2, 3, 4, 5, 6, 7])  # no answer to the notification or the empty line
        self.assertTrue(r[6]["result"]["isError"])  # params that are not an object: an error, and the server goes on
        self.assertEqual(r[0]["result"]["protocolVersion"], "2025-03-26")
        self.assertEqual(r[0]["result"]["serverInfo"]["version"], "9.9")
        self.assertEqual(r[1]["error"]["code"], -32700)
        self.assertEqual([t["name"] for t in r[2]["result"]["tools"]], [t["name"] for t in tools.TOOLS])
        self.assertEqual(r[3]["result"], {"content": [{"type": "text", "text": "status {}"}], "isError": False})
        self.assertEqual((r[4]["result"]["content"][0]["text"], r[4]["result"]["isError"]), ("window gone", True))
        self.assertEqual(r[5]["error"]["code"], -32601)


class Tools(unittest.TestCase):
    def setUp(self):
        self.store = Store(tempfile.mkdtemp())
        self.ctl = SimpleNamespace(store=self.store, key="k", premium=False, lock=threading.Lock(), jobs=[],
                                   add_links=mock.Mock(), auto_sort=lambda: "sorted")

    def run_tool(self, tool, **args):
        return tools.run(self.ctl, json.dumps({"tool": tool, "args": args}))

    def test_add_mods_takes_only_mod_links_and_says_what_the_user_must_do(self):
        links = ["https://www.nexusmods.com/cyberpunk2077/mods/107", "", "https://www.youtube.com/watch?v=x",
                 "https://example.com/x.zip", r"\\host\share\x.zip", "C:/Downloads/x.zip", "https://github.com/wopss/RED4ext",
                 "https://github.com/someone/cp2077-thing"]
        with mock.patch("os.path.isfile") as isfile:
            text = self.run_tool("add_mods", links="\n".join(links))["text"]
        self.assertNotIn(mock.call(r"\\host\share\x.zip"), isfile.call_args_list)  # looking at it logs in to that host
        self.ctl.add_links.assert_called_once_with("https://www.nexusmods.com/cyberpunk2077/mods/107\nhttps://github.com/wopss/RED4ext")
        for part in ("Added 2 link(s)", "Not added:", "find_mods_in_video", "folder is not set", "Slow download",
                     "Not a Nexus Mods link: https://example.com/x.zip", "Not a Nexus Mods link: https://github.com/someone",
                     r"Not a web link: \\host", "Not a web link: C:/"):
            self.assertIn(part, text)

    def test_status_sort_and_bad_calls(self):
        self.store.mods = [mod("a", [], name="Alpha"), mod("b", [], name="Beta", enabled=False)]
        self.ctl.jobs = [{"name": "Gamma", "status": "Click 'Slow download'", "state": "waiting"}]
        text = self.run_tool("status")["text"]
        for part in ("free account", "- Gamma:", "Waiting files", "1. Alpha", "2. Beta (off)"):
            self.assertIn(part, text)
        self.assertEqual(self.run_tool("sort_load_order"), {"text": "sorted"})
        self.assertEqual(self.run_tool("format_disk"), {"error": "Unknown tool: format_disk"})
        self.assertIn("error", self.run_tool("status", extra=1))
        self.assertIn("error", tools.run(self.ctl, "{not json"))

    def test_the_home_folder_never_goes_to_the_ai_app(self):
        bad = str(Path.home()) + "\\x\\a.zip"
        self.ctl.jobs = [{"name": "M", "status": f"Failed: {bad} {bad!r} {bad.replace(chr(92), '/')}", "state": "failed"}]
        text = self.run_tool("status")["text"]
        for form in (str(Path.home()), str(Path.home()).replace("\\", "\\\\"), str(Path.home()).replace("\\", "/")):
            self.assertNotIn(form.lower(), text.lower())
        self.assertIn("~\\x\\a.zip", text)


class LocalAi(unittest.TestCase):
    def test_advice_keeps_only_real_picks(self):
        mods = [mod("patch", [], name="A - Patch for B"), mod("a", [], name="A"), mod("b", [], name="B")]
        probs = [doctor.problem("p1", "t", "d", [doctor.fix("Update A", "update", "a"), doctor.fix("Turn A off", "disable", "a")], "high"),
                 doctor.problem("p2", "t2", "d2", [doctor.fix("Keep", "ignore", "p2")], "low")]
        answers = [{"builds_on": {"1": [2, 3, 1, 9, 0, "2", True], "2": [], "3": "1", "x": [1], "4": [1]}},
                   {"summary": " S ", "fixes": [{"problem": 1, "fix": 2, "why": "w"}, {"problem": 1, "fix": 1, "why": ""},
                                                 {"problem": 2, "fix": 5, "why": ""}, {"problem": 3, "fix": 1, "why": ""},
                                                 {"problem": True, "fix": 1, "why": ""}]}]
        with mock.patch.object(llm, "chat", side_effect=answers) as chat:
            a = llm.advise(mods, probs)
        self.assertEqual(a["after"], [("a", "patch"), ("b", "patch")])
        self.assertEqual([(p["id"], f["label"], why) for p, f, why in a["fixes"]], [("p1", "Turn A off", "w")])
        self.assertEqual(a["summary"], "S")
        first = chat.call_args_list[0].args
        self.assertIn("1. A - Patch for B", first[1])
        self.assertEqual(first[2]["properties"]["builds_on"]["required"], ["1", "2", "3"])
        self.assertIn("P2 (low) t2: d2", chat.call_args_list[1].args[1])

    def test_bug_report_gives_a_cause_and_only_real_mod_fixes(self):
        mods = [mod("car", [], name="Car", enabled=False), mod("b", [], name="B")]
        answers = [{"builds_on": {}}, {"summary": "s", "fixes": [], "cause": " The car is off. ", "mod_fixes": [
            {"mod": 1, "action": "turn_on", "why": "w"}, {"mod": 3, "action": "turn_on", "why": ""},
            {"mod": 2, "action": "delete", "why": ""}, {"mod": True, "action": "update", "why": ""}, "x"]}]
        with mock.patch.object(llm, "chat", side_effect=answers) as chat:
            a = llm.advise(mods, [], "My car is missing")
        self.assertEqual(a["cause"], "The car is off.")
        self.assertEqual([(m["id"], act, why) for m, act, why in a["mod_fixes"]], [("car", "turn_on", "w")])
        system, user, schema = chat.call_args_list[1].args
        self.assertIn("describes a bug", system)
        self.assertIn("Problems found:\nnone", user)
        self.assertIn("The player says: My car is missing", user)
        self.assertIn("mod_fixes", schema["required"])

    def test_no_problems_asks_only_about_the_order(self):
        with mock.patch.object(llm, "chat", return_value={"builds_on": []}) as chat:
            a = llm.advise([mod("a", [])], [])
        self.assertEqual(chat.call_count, 1)
        self.assertEqual(a, {"summary": "The Crash doctor found no problems.", "fixes": [], "after": []})

    def test_a_download_with_the_wrong_hash_is_deleted(self):
        d = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, d)
        f = d / "x.bin"
        f.write_bytes(b"abc")
        good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        self.assertEqual(llm.checked(f, good), f)
        with self.assertRaises(RuntimeError):
            llm.checked(f, "0" * 64)
        self.assertFalse(f.exists())

    def test_start_needs_the_files_and_stop_ends_the_engine(self):
        with (mock.patch.object(llm, "ready", return_value=False), mock.patch.object(llm, "_server", None),
              mock.patch.object(llm, "_blocked", False), self.assertRaises(RuntimeError, msg="files are missing")):
            llm.start()
        proc = mock.Mock()
        with mock.patch.object(llm, "_server", proc):
            llm.stop()
            self.assertIsNone(llm._server)
        proc.kill.assert_called_once_with()
        llm.stop()  # nothing runs: no error

    def test_a_blocked_engine_does_not_start(self):
        with mock.patch.object(llm, "_blocked", False), mock.patch.object(llm, "_server", None),                 mock.patch.object(llm.subprocess, "Popen") as popen:
            llm.block()
            with self.assertRaises(RuntimeError) as e:
                llm.start()
            self.assertEqual(str(e.exception), llm.BLOCKED)
            popen.assert_not_called()

    def test_the_game_starts_while_the_model_loads(self):
        proc = mock.Mock()
        proc.poll.return_value = None

        def health(*a, **k):  # the game starts while the engine loads the model
            llm.block()
            return {"status": "ok"}
        with mock.patch.object(llm, "_blocked", False), mock.patch.object(llm, "_server", None),                 mock.patch.object(llm, "ready", return_value=True), mock.patch.object(llm.subprocess, "Popen", return_value=proc),                 mock.patch.object(llm, "http_json", health):
            with self.assertRaises(RuntimeError) as e:
                llm.start()
            self.assertEqual(str(e.exception), llm.BLOCKED)
            proc.kill.assert_called_once_with()  # the engine does not run next to the game
            self.assertIsNone(llm._server)

    def test_chat_asks_for_json_without_thinking(self):
        reply = {"choices": [{"message": {"content": '{"a": 1}'}}]}
        with mock.patch.object(llm, "http_json", return_value=reply) as h, mock.patch.object(llm, "KEY", "k1"):
            self.assertEqual(llm.chat("sys", "user", {"type": "object"}), {"a": 1})
        body = h.call_args.args[1]
        self.assertTrue(h.call_args.args[0].endswith("/v1/chat/completions"))
        self.assertEqual(body["response_format"]["json_schema"]["schema"], {"type": "object"})
        self.assertFalse(body["chat_template_kwargs"]["enable_thinking"])
        self.assertEqual([m["role"] for m in body["messages"]], ["system", "user"])
        self.assertEqual(h.call_args.args[2], {"Authorization": "Bearer k1"})  # the engine only answers the app


if __name__ == "__main__":
    unittest.main()
