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
        patches = [mock.patch.object(app, "PIPE", self.pipe), mock.patch.object(app, "TOKEN", self.token),
                   mock.patch.object(sys, "argv", ["app.py", LINK])]
        for p in patches:
            p.start()
            self.addCleanup(p.stop)

    def test_running_window_takes_the_link(self):
        self.token.write_bytes(b"secret")
        got = []

        def window():
            with Listener(self.pipe, family="AF_PIPE", authkey=b"secret") as ls, ls.accept() as c:
                got.append(c.recv_bytes().decode())

        t = threading.Thread(target=window, daemon=True)
        t.start()
        time.sleep(0.2)
        with mock.patch("manager_ui.ui.run") as run:
            app.main()
        t.join(3)
        run.assert_not_called()  # no second window
        self.assertEqual(got, [LINK])

    def test_pipe_holder_that_never_answers_cannot_hang_start_up(self):
        self.token.write_bytes(b"secret")
        silent = Listener(self.pipe, family="AF_PIPE")
        self.addCleanup(silent.close)
        threading.Thread(target=silent.accept, daemon=True).start()  # takes the call, then says nothing
        t = time.perf_counter()
        self.assertFalse(app.hand_over(LINK, timeout=1))
        self.assertLess(time.perf_counter() - t, 3)

    def test_link_refused_by_a_closing_window_opens_the_next_window(self):
        closing = Listener(self.pipe, family="AF_PIPE")  # holds the name, answers nobody (a window that is closing)
        threading.Timer(1.0, closing.close).start()  # ... and is gone after 1 s
        with mock.patch("manager_ui.ui.run") as run:
            app.main()
        run.assert_called_once()
        server, url = run.call_args[0][:2]
        self.assertEqual(url, LINK)  # the link was kept
        self.assertTrue(self.token.read_bytes())  # this process wrote its own token after taking the pipe
        server.close()


if __name__ == "__main__":
    unittest.main()
