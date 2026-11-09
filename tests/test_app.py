"""Start-up hand-over in app.main, on a private test pipe; the window itself is replaced by a stub."""
import sys
import tempfile
import threading
import time
import unittest
import uuid
from multiprocessing.connection import Listener
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

        def refuse():  # like ui._take while the window closes: accept, then hang up at once
            while True:
                try:
                    closing.accept().close()
                except OSError:
                    return

        threading.Thread(target=refuse, daemon=True).start()
        threading.Timer(1.0, closing.close).start()  # the closing window is gone after 1 s
        app.main()
        self.run_window.assert_called_once()
        server, url = self.run_window.call_args[0][:2]
        self.assertEqual(url, LINK)  # the link was kept
        self.assertNotEqual(self.token.read_bytes(), b"secret")  # this window wrote its own token after taking the pipe
        server.close()


if __name__ == "__main__":
    unittest.main()
