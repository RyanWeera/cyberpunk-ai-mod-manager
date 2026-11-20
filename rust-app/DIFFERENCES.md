# Differences between the Rust app and the Python app

The Rust app has the same features as the Python app (v1.3.5). It reads and writes the same data folder
(`CP2077MM_DATA`, default `%LOCALAPPDATA%\CP2077ModManager`) and the same `state.json`, so a user can switch
between the two apps without losing mods. This file lists every place where the Rust app behaves differently.

## Bugs fixed (Python v1.3.3 had them)
- Automatic Slow download click: four bugs stopped it from working. Unmasked WebSocket frames never completed (FIN
  was read from the mask bit), `WS.recv` did not read the socket, `HOOK_JS` had a JavaScript syntax error, and
  `READY_JS` accepted a new tab while it was still `about:blank`. Python v1.3.4 and v1.3.5 have the same fixes. A real hidden-Brave test passes in both apps.
- The DevTools PUT that opens a tab now sends a `Content-Length` (Rust only: the browser closes the connection
  without one).

## Stricter or safer
- The install file walk does not go into symlinks or junctions (Python 3.12 `rglob` goes into junctions).
- `find_browser` skips empty or relative environment variables. Python could look in the current folder.
- `dbghelp.dll` is loaded from System32 by its full path.
- Video page redirects are followed by hand (10 at most), and each one is checked as a public address.
  A status that is not 2xx is an error.
- `Browser::start` deletes an old `DevToolsActivePort` file first, and waits while the file is incomplete.

## Different method, same result
- The game process is found from the Windows process list (Toolhelp32), not with `tasklist.exe`.
- Mod thumbnails are made with the `image` crate (PNG, JPEG and WebP), not PowerShell/WPF. File names stay the same.
- FOMOD files are decoded by their byte order mark (UTF-16 LE/BE or UTF-8). The `<!ENTITY` check also runs on the
  decoded text, so it also catches UTF-16 BE files.
- tar has a 30-minute time limit. The old-Windows hint reads `CurrentBuildNumber` from the registry.
- A file that Defender blocked (os error 225/226) or that Windows refused to read (os error 87) is found from the
  error code. Python used exception types.
- The compatibility-patch check (24 candidates at most) runs in batches of 6 threads.
- The local AI engine key comes from the standard library random state.

## Small visible differences
- Some error text differs: XML errors use the roxmltree message, file errors read `<os error>: '<path>'`, and
  tar and Defender output are decoded as UTF-8, not the ANSI code page. A non-ASCII path in Defender's last
  line can therefore stay visible in the log.
- A second Nexus login prompt fails with "the Nexus page still asks login". Python gave an empty message.
- An nxm link with no `expires` sends `expires=`. Python sent `expires=None`.
- A whole-number `crash_secs` is saved as `300`, not `300.0`. Both apps read both forms.
- Crash report zip entries use local time.
- If `wevtutil` cannot start, the crash evidence has no events. Python stopped with an error.
- After a clean game exit, the watcher calls its exit step even when it cannot delete the snapshot.
- In Launch, the mod list refresh and the "using pack" log line come after the deploy, not before it.
- In the bad-mod search, the start step runs after the history is saved.
- Downloads have no limit on the total time, only 60 seconds for each read (the same as Python's socket timeout).
- The app always adds the Advanced Crash Reporter mod (owner decision 2026-11-21): after the Nexus key check and
  before each launch, when it is not in the game folder, the mod list or the download queue. It is queued once per
  run, so a skipped download comes back only after a restart. When it is in the mod list but off, the app turns it on
  again, and a mod pack never turns it off, also a pack saved while it was off. The crash doctor never blames it
  (it is the crash handler, so it is on every crash stack) and never offers "Disable" for it, nor does the local AI's
  bug report. The bad-mod search keeps it on in every round, and a folder that holds only its `reports` does not
  count as installed. A launch only queues the download: it works from the next game start. Python only offered it after a crash with no clue.

## Window and start-up
- The window uses egui, not Tkinter. The rail is a left side panel, and the Add dialog and the questions (save
  name, merge, pack choice, outdated) are modal dialogs. Every control and label meaning stays.
- The Cyberpunk style adds a cyan second colour (#00E5FF) beside the yellow #FCEE0A. Arasaka and Militech keep
  the Python colours. The chosen style is kept in the same setting.
- Yes/no, info and error boxes are Windows message boxes, and "No" is the default where Python had "No" as the
  default. File and folder pickers are the Windows dialogs.
- Fonts load by full path from `%SystemRoot%\Fonts`. egui handles DPI, so there is no Tk DPI call.
- The exe is `cp2077-ai-mod-manager.exe`, because a Cargo program name cannot hold spaces. The release step must
  rename it.
- When a second copy hands a link over and the pipe is busy, it waits up to 1 second (WaitNamedPipe). Python's
  client retried in a loop.
- The window does not write the nxm:// registration at start-up. Only the Settings button and the waiting-download
  question write it, as in Python.

## Removed because it has no meaning in a Rust build
- Update always swaps the exe. The `git pull` path for a run from source is gone, and so is
  `PYINSTALLER_RESET_ENVIRONMENT`.

## Tests
- Each Python test has one Rust test with a comment that names the Python test. `mock.patch` seams became
  parameters (`Deps` in the controller, `fetch::set_mock`, `set_resolver`, an `Os` trait for the game watcher).
- `tests/test_code_map.py` tests a Python developer script and is not ported. `tests/ai_bench.py` (real local AI
  benchmark) is not ported: run it with Python.
- New Rust tests: every page and dialog renders headless in each style, `--mcp` answers on stdio from the built
  exe, the WebSocket frames from a real socket, and a real hidden-browser Slow download click (ignored by default).

## Version
- The Rust app is version 2.0.0. It has every change of Python v1.3.4 and v1.3.5 (the WebSocket fixes and the
  plain-text tab close answer, with the same tests). The Python app's Update button finds the v2.0.0 exe on GitHub
  Releases like any other release, so Python users move to the Rust app with their mods and settings.
