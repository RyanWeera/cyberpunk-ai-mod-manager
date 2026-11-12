"""The AI-app link (MCP server and its tools) and the local AI's answer checks, with no real AI app or model."""
import io
import json
import os
import shutil
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from ai_link import mcp, tools
from crash_doctor import doctor
from local_ai import ollama
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


class LocalAi(unittest.TestCase):
    def test_advice_keeps_only_real_picks(self):
        mods = [mod("patch", [], name="A - Patch for B"), mod("a", [], name="A"), mod("b", [], name="B")]
        probs = [doctor.problem("p1", "t", "d", [doctor.fix("Update A", "update", "a"), doctor.fix("Turn A off", "disable", "a")], "high"),
                 doctor.problem("p2", "t2", "d2", [doctor.fix("Keep", "ignore", "p2")], "low")]
        answers = [{"builds_on": {"1": [2, 3, 1, 9, 0, "2", True], "2": [], "3": "1", "x": [1], "4": [1]}},
                   {"summary": " S ", "fixes": [{"problem": 1, "fix": 2, "why": "w"}, {"problem": 1, "fix": 1, "why": ""},
                                                 {"problem": 2, "fix": 5, "why": ""}, {"problem": 3, "fix": 1, "why": ""},
                                                 {"problem": True, "fix": 1, "why": ""}]}]
        with mock.patch.object(ollama, "chat", side_effect=answers) as chat:
            a = ollama.advise("m", mods, probs)
        self.assertEqual(a["after"], [("a", "patch"), ("b", "patch")])
        self.assertEqual([(p["id"], f["label"], why) for p, f, why in a["fixes"]], [("p1", "Turn A off", "w")])
        self.assertEqual(a["summary"], "S")
        first = chat.call_args_list[0].args
        self.assertIn("1. A - Patch for B", first[2])
        self.assertEqual(first[3]["properties"]["builds_on"]["required"], ["1", "2", "3"])
        self.assertIn("P2 (low) t2: d2", chat.call_args_list[1].args[2])

    def test_no_problems_asks_only_about_the_order(self):
        with mock.patch.object(ollama, "chat", return_value={"builds_on": []}) as chat:
            a = ollama.advise("m", [mod("a", [])], [])
        self.assertEqual(chat.call_count, 1)
        self.assertEqual(a, {"summary": "The Crash doctor found no problems.", "fixes": [], "after": []})

    def test_has_model_adds_latest(self):
        with mock.patch.object(ollama, "http_json", return_value={"models": [{"name": "llama3.2:latest"}, {"name": "qwen3.5:4b"}]}):
            self.assertTrue(ollama.has_model("llama3.2"))
            self.assertTrue(ollama.has_model("qwen3.5:4b"))
            self.assertFalse(ollama.has_model("qwen3.5:9b"))

    def test_ollama_is_never_taken_from_the_current_folder(self):
        here = tempfile.mkdtemp()
        Path(here, "ollama.exe").write_bytes(b"")
        self.addCleanup(shutil.rmtree, here)
        self.addCleanup(os.chdir, os.getcwd())  # runs first: a folder in use cannot be removed
        os.chdir(here)
        with mock.patch.object(ollama, "HOME", Path(here, "none")):
            with mock.patch.dict(os.environ, {"PATH": ";.;"}):  # empty and relative PATH entries mean this folder too
                self.assertIsNone(ollama.exe())
            with mock.patch.dict(os.environ, {"PATH": here}):
                self.assertEqual(ollama.exe(), str(Path(here, "ollama.exe")))

    def test_pull_progress_counts_every_file(self):
        lines = [{"digest": "a", "total": 90, "completed": 90}, {"digest": "b", "total": 10, "completed": 5}]
        seen = []
        with mock.patch.object(ollama.urllib.request, "urlopen") as urlopen:
            urlopen.return_value.__enter__.return_value = [json.dumps(d).encode() for d in lines]
            ollama.pull("m", seen.append)
        self.assertEqual(seen, [100, 95])  # the small second file does not start the bar again at 0


if __name__ == "__main__":
    unittest.main()
