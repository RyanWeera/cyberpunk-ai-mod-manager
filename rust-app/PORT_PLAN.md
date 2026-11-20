# Rust port plan

The Python app (repo root) is the spec. The Rust app in this folder must have the same features and read and write
the same data folder (`CP2077MM_DATA`, default `%LOCALAPPDATA%\CP2077ModManager`) and the same `state.json`.
The Python app stays, and keeps the GitHub Releases, until the Rust app has every feature.

## Decisions (2026-11-20)
- GUI: egui/eframe, with a custom Cyberpunk theme (owner: "make it look good and user friendly").
- Toolchain: rustup stable, target `x86_64-pc-windows-gnu`, with MinGW-w64 (winlibs) in `%USERPROFILE%\tools\mingw64`
  for `dlltool`/`as` (no Visual Studio needed).
- One Cargo workspace, one crate per Python feature folder, so one crate that does not compile never blocks the others.
- Errors: public fallible functions return `Result<T, String>` (a message for people), unless a caller must match on the kind.
- Free-form records that the Python app keeps as dicts stay `serde_json::Value` (link/job descriptions, `Mod.source`,
  `Mod.requires`, `store.doctor`, `pack_info`, problems and fixes). `Mod` and `Settings` are typed, with a
  `#[serde(flatten)] extra` map so unknown fields survive a save.
- Tests share global state (HTTP mock, env vars) like the Python suite, so `.cargo/config.toml` sets `RUST_TEST_THREADS=1`.

## Build environment
```
export PATH="$HOME/.cargo/bin:$HOME/tools/mingw64/bin:$PATH"   # Git Bash
cargo test -p <crate>
cargo clippy -p <crate> --all-targets -- -D warnings
```

## Crates and order
| Step | Crate | Python source | Python tests | Depends on |
|---|---|---|---|---|
| 1 | `shared` | `shared/store.py` (+ `FRAMEWORKS` from `downloads/fetch.py`) | (none in Python; 8 new) | - |
| 1 | `testkit` | `tests/helpers.py` | - | shared |
| 2 | `installer` | `installer/install.py`, `deploy.py` | `test_install.py` (18), `test_deploy.py` (17) | shared |
| 2 | `downloads` | `downloads/fetch.py`, `nexus.py`, `nexus_browser.py` | `test_links.py` (13, minus UI parts), `test_browser.py` (17, minus `Wiring`) | shared |
| 2 | `virus_scan` | `virus_scan/scan.py` | `test_virus_scan.py` (1) | - |
| 2 | `crash_doctor` | `crash_doctor/doctor.py`, `watch.py` | `test_doctor.py` (31), `e2e_crash.py` | shared |
| 3 | `video_finder` | `video_finder/finder.py` | `test_video.py` (12) | downloads |
| 3 | `local_ai` | `local_ai/llm.py` | `test_ai.py` `LocalAi` | downloads |
| 3 | `ai_link` | `ai_link/mcp.py`, `tools.py` | `test_ai.py` `Mcp`, `Tools` | downloads, video_finder |
| 4 | `manager_ui` controller | `manager_ui/controller.py` | `test_controller.py` (28), `test_browser.py` `Wiring` | all |
| 5 | `manager_ui` window + exe | `manager_ui/ui.py`, `app.py` | `test_ui.py` (23), `test_app.py` (8) | all |

`tests/test_code_map.py` tests a Python developer script and is not ported. `tests/ai_bench.py` (real model benchmark)
is not ported: it runs with Python.

## Shared interfaces (fixed before the parallel steps)
- `shared`: `Store`, `Mod`, `Settings`, `kind_of`, `find_game`, `data_dir`, `sys32`, `FRAMEWORKS`, `fw_by_nexus`,
  `fw_by_repo`, `VERSION`, `GAME_EXE`, `pretty` (Python `json.dumps(indent=1)`), `win::{reg_get, reg_set, wide}`.
  Callers keep one `Store` in `Arc<Mutex<Store>>`; Rust has no re-entrant lock, so never lock it twice on one thread.
- `downloads::fetch` owns all HTTP. Every request in the workspace (Nexus, GitHub, video pages, the local AI engine)
  goes through `fetch::send(&Request) -> Result<Response, String>`:
  `Request { method, url, headers: Vec<(String, String)>, body: Option<Vec<u8>>, timeout_secs, redirects }`
  (`redirects` = how many redirects to follow; 0 returns the 3xx response so the caller can check `Location`),
  `Response { status, url (final), headers: Vec<(String, String)>, body: Vec<u8> }`.
  Tests replace the network with `fetch::set_mock(Some(Arc<dyn Fn(&Request) -> Result<Response, String> + Send + Sync>))`
  and `fetch::set_mock(None)`; with a mock set, nothing reaches the network.
- Tests: one `#[test]` per Python test method, same name in snake_case, with a comment naming the Python test
  (`// test_install.py Install.test_fomod_defaults`). Use `testkit` helpers.

## Rules to keep (from the Python app)
- Unpack with `%SystemRoot%\System32\tar.exe` (bsdtar), archive on stdin. Scan with Defender `MpCmdRun.exe`.
  Full program paths only; never a PATH or current-folder lookup. Never put a downloaded or user-chosen file name on a
  command line. Child processes use `CREATE_NO_WINDOW`.
- Mod list index 0 is the lowest priority. Deploy writes `archive/pc/mod/modlist.txt` in reverse.
- Free Nexus accounts: the hidden Brave/Edge/Chrome browser clicks "Slow download" over DevTools (owner decision
  2026-11-20), 3-second gap between clicks, every failure falls back to a manual click; non-Edge browsers use
  `browser/<name>` profile subfolders.
- After "Complete installation" the app uses the pack automatically.
- Secrets: the Nexus API key, `ipc.token`, the `browser` profile folder. Never print, log or commit them.
- Text from web pages or AI apps is untrusted: video pages go through `finder::public`, FOMOD paths through
  `install::inside`, never blame a mod for files in shared game folders (`doctor::SHARED`).

## Status
- [x] Step 1: `shared` (8 tests), `testkit`
- [x] Step 2a: `installer` (35 tests), `downloads` (28 tests + 1 ignored real-browser test; window test `short`/`ago` and `Wiring` go to steps 4-5)
- [x] Step 2b: `virus_scan` (1 test), `crash_doctor` (31 tests; `e2e_crash.py` goes to step 4)
- [x] Step 3: `video_finder` (11), `local_ai` (8), `ai_link` (4); `test_errors_reach_the_window` goes to step 4
- [x] Step 4: controller (32 tests; `e2e_crash.rs` passes as an ignored test)
- [x] Step 5: window, exe, single instance, nxm:// links, `--mcp` (manager_ui 66 tests + `--mcp` stdio test)
- [x] Differences report (`DIFFERENCES.md`)
- [x] Full check (2026-11-21): `cargo test --workspace` 199 passed, clippy clean, ignored e2e_crash and
  real_browser tests pass, release exe builds and opens

## Python bugs found during the port (fixed in Rust; released in Python v1.3.4 and v1.3.5)
- `downloads/nexus_browser.py`: `ws_message` took `fin` from the mask bit, so unmasked server text frames never
  completed; `WS.recv` did not read the socket when no frame was buffered; `HOOK_JS` had `o?o.apply(...)` with no
  `:null` (a JavaScript syntax error); `READY_JS` counted a new tab as ready while it was still `about:blank`, so the
  click found no link. Together these meant the automatic Slow download click in v1.3.3 never worked.
- Rust only: the DevTools PUT that opens a tab needs a `Content-Length`, or the browser closes the connection.
