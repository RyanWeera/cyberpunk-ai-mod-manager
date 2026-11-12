"""Start-up hand-over in app.main, on a private test pipe; the window itself is replaced by a stub."""
import ctypes
import json
import sys
import tempfile
import threading
import time
import unittest
import uuid
from multiprocessing.connection import Client, Listener
from pathlib import Path
from unittest import mock

import app

LINK = "nxm://cyberpunk2077/mods/1/files/2?key=a&expires=1"


class StartUp(unittest.TestCase):
    def setUp(self):
        self.pipe = rf"\\.\pipe\cp2077mm-test-{uuid.uuid4().hex}"
        self.token = Path(tempfile.mkdtemp()) / "ipc.token"
        self.token.write_bytes(b"secret")
        patches = [mock.patch.object(app, "PIPE", self.pipe), mock.patch.object(app, "TOKEN", self.token),
                   mock.patch.object(sys, "argv", ["app.py", LINK]), mock.patch("manager_ui.ui.run")]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)
        import manager_ui.ui
        self.run_window = manager_ui.ui.run

    def window(self, got, delay=0.0):
        """A running window: proves the token, takes links, confirms with "ok" (like ui._take)."""
        ls = Listener(self.pipe, family="AF_PIPE", authkey=b"secret")
        self.addCleanup(ls.close)

        def serve():
            time.sleep(delay)
            while True:
                try:
                    with ls.accept() as c:
                        got.append(c.recv_bytes().decode())
                        c.send_bytes(b"ok")
                except OSError:
                    return

        threading.Thread(target=serve, daemon=True).start()

    def test_ai_tool_call_goes_to_the_window(self):
        ls = Listener(self.pipe, family="AF_PIPE", authkey=b"secret")
        self.addCleanup(ls.close)
        got = []

        def serve():
            for reply in ({"text": "3 mods"}, {"error": "busy"}):
                with ls.accept() as c:
                    got.append(json.loads(c.recv_bytes()))
                    c.send_bytes(json.dumps(reply).encode())

        threading.Thread(target=serve, daemon=True).start()
        with mock.patch.object(app.subprocess, "Popen") as popen:  # a slow test thread must not start a real window
            self.assertEqual(app.ask_window("status", {}), "3 mods")
            with self.assertRaisesRegex(RuntimeError, "^busy$"):
                app.ask_window("sort_load_order", {})
        popen.assert_not_called()
        self.assertEqual(got, [{"tool": "status", "args": {}}, {"tool": "sort_load_order", "args": {}}])

    def test_ai_tool_call_starts_the_window_once(self):
        with mock.patch.object(app.subprocess, "Popen") as popen:
            with self.assertRaisesRegex(RuntimeError, "did not start"):
                app.ask_window("status", {}, wait=1.2)
        popen.assert_called_once()  # tried again until the deadline, but started only one copy
        self.assertEqual(popen.call_args.kwargs["env"]["PYINSTALLER_RESET_ENVIRONMENT"], "1")

    def test_ai_tool_call_starts_no_copy_while_a_window_runs(self):
        for err in (OSError(0, "All pipe instances are busy", None, 231), app.AuthenticationError("digest was wrong")):
            with mock.patch.object(app, "Client", side_effect=err), mock.patch.object(app.subprocess, "Popen") as popen:
                with self.assertRaisesRegex(RuntimeError, "did not answer"):
                    app.ask_window("status", {}, wait=0.6)
            popen.assert_not_called()  # a second copy would only show "still running"

    def test_window_pipe_is_open_to_this_account_only(self):
        server = app.OwnerListener(self.pipe)
        self.addCleanup(server.close)
        adv, sd, sddl = ctypes.WinDLL("advapi32"), ctypes.c_void_p(), ctypes.c_wchar_p()
        adv.GetSecurityInfo.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_uint32] + [ctypes.c_void_p] * 5
        dacl = 4  # DACL_SECURITY_INFORMATION; 6 below is SE_KERNEL_OBJECT
        self.assertEqual(adv.GetSecurityInfo(server._listener._handle_queue[0], 6, dacl, None, None, None, None,
                                             ctypes.byref(sd)), 0)
        self.assertTrue(adv.ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, 1, dacl, ctypes.byref(sddl), None))
        self.assertEqual(sddl.value, "D:P(A;;FA;;;OW)")  # the owner only: no Everyone (WD) or Anonymous (AN) read
        got = []
        t = threading.Thread(target=lambda: got.append(server.accept().recv_bytes()), daemon=True)
        t.start()
        with Client(self.pipe, family="AF_PIPE") as c:  # this account still gets in
            c.send_bytes(b"show")
        t.join(5)
        self.assertEqual(got, [b"show"])

    def test_running_window_takes_the_link(self):
        got = []
        self.window(got)
        app.main()
        self.run_window.assert_not_called()  # no second window
        self.assertEqual(got, [LINK])

    def test_slow_window_gets_the_link_once(self):
        got = []
        self.window(got, delay=6.5)  # takes the pipe, then builds its UI for 6.5 s before serving
        app.main()
        time.sleep(1.5)  # a second attempt would arrive now
        self.run_window.assert_not_called()
        self.assertEqual(got, [LINK])

    def test_pipe_holder_that_never_answers_cannot_hang_start_up(self):
        silent = Listener(self.pipe, family="AF_PIPE")
        self.addCleanup(silent.close)
        threading.Thread(target=silent.accept, daemon=True).start()  # takes the call, then says nothing
        t = time.perf_counter()
        with mock.patch("ctypes.windll.user32.MessageBoxW") as box:
            app.main(wait=2)
        self.assertLess(time.perf_counter() - t, 5)
        box.assert_called_once()  # "still running and did not answer"
        self.run_window.assert_not_called()

    def test_link_refused_by_a_closing_window_opens_the_next_window(self):
        closing = Listener(self.pipe, family="AF_PIPE")
        tries = []

        def refuse():  # like ui._take while the window closes: accept, then hang up at once
            while True:
                try:
                    closing.accept().close()
                    tries.append(1)
                except OSError:
                    return

        threading.Thread(target=refuse, daemon=True).start()
        threading.Timer(1.0, closing.close).start()  # the closing window is gone after 1 s
        app.main()
        self.assertLessEqual(len(tries), 4)  # paced retries (about 2 a second), not a busy loop
        self.run_window.assert_called_once()
        server, url = self.run_window.call_args[0][:2]
        self.assertEqual(url, LINK)  # the link was kept
        self.assertNotEqual(self.token.read_bytes(), b"secret")  # this window wrote its own token after taking the pipe
        server.close()


if __name__ == "__main__":
    unittest.main()
