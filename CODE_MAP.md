# Code Map

A guide for an AI (or a person) that changes this code. Read this file first, then open only the file and lines you need.
The index at the end lists every module, class, function and constant with its line number (`L123`).
After you change code, run `python tools/code_map.py` so the line numbers stay right.

## The app in one paragraph
CP2077 AI Mod Manager is a Windows desktop app for Cyberpunk 2077 mods (version in `shared/store.py`, `VERSION`).
The user pastes Nexus or GitHub links (or picks files). The app downloads each mod, scans it with Microsoft Defender,
unpacks it into a staging folder and copies the enabled mods into the game folder. It watches the game, and after a
crash or a freeze it finds the cause and offers fixes. A small local AI (llama.cpp with a Qwen model) can pick fixes
and sort the load order. An MCP server lets other AI apps (Claude, Cursor) add and check mods through the window.

## Hard rules
1. Python 3.12+, standard library only. Do not add a pip package. The window is Tkinter.
2. Only `manager_ui/ui.py` may touch Tk. All other code runs without a window.
3. Worker threads never call the window. They call `notify(kind, payload)`; `App._drain` in `ui.py` reads the queue
   every 150 ms and updates the window. To show something new: add a `kind` in the controller and a branch in `_drain`.
4. Mod list order: `store.mods[0]` is the lowest priority. The last mod wins every conflict.
   Deploy writes `archive/pc/mod/modlist.txt` in reverse order, because the game loads the first archive first.
5. Always call Windows programs by full path (`SYS32`, `TAR` constants). Never use `shutil.which`.
   Never put a downloaded or user-chosen file name on a command line (tar reads from stdin; downloads get `ascii_name`).
6. Never print, log or commit the Nexus API key (`store.settings["api_key"]`) or `ipc.token`.
7. Never automate the Nexus "Slow download" click for free accounts. The user clicks; Nexus sends an `nxm://` link.
8. Any link or text that comes from a web page or an AI app is untrusted. Video pages go through `finder.public()`.
   FOMOD paths go through `install._inside()`. Never blame a mod for files in shared game folders (`doctor.SHARED`).
9. Keep changes small. Follow the style around the code you change. Add or update a test in `tests/`.

## Commands
- Run from source: `python app.py`
- All tests: `python -m unittest discover -s tests -t .`
- One test file: `python -m unittest tests.test_install` (file names are in the index, section `tests/`)
- Lint: `uvx ruff check --select F,E9,B --ignore B023 .`
- Real crash check (Windows, about 1 minute): `python -u tests/e2e_crash.py`
- Refresh this map: `python tools/code_map.py`

## Folders
| Folder | Job | Entry point |
|---|---|---|
| `app.py` | Start-up: one window only, `nxm://` links from the browser, `--mcp` mode | `main` |
| `manager_ui/` | `ui.py` = the Tk window (pages, buttons). `controller.py` = all actions, no Tk | `ui.run`, `Controller` |
| `downloads/` | Link parsing, Nexus API (GraphQL + v1), GitHub releases, file download, framework list | `nexus.parse_link` |
| `installer/` | Unpack (tar.exe), FOMOD defaults, map files to game folders, deploy to the game | `install.install`, `deploy.deploy` |
| `crash_doctor/` | `watch.py` = game process, crash and freeze evidence. `doctor.py` = problems, fixes, load order | `doctor.diagnose` |
| `local_ai/` | Local model in llama-server: start, stop, ask for fixes and load-order rules | `llm.advise` |
| `ai_link/` | MCP server for AI apps; tool list and calls | `mcp.serve`, `tools.run` |
| `video_finder/` | Find mods a YouTube, TikTok or Instagram video shows, from its text | `finder.find` |
| `virus_scan/` | Microsoft Defender scan | `scan.scan` |
| `shared/` | Settings and mod list in one JSON file, game folder search | `Store`, `find_game` |
| `tests/` | unittest suite with fake game folders (`helpers.py`, `fake_game.py`) | - |
| `tools/` | Developer scripts (this map) | `code_map.py` |

## How the main flows run

### Add a mod (link -> installed)
1. `ui.App.add_links` -> `Controller.add_links` -> (background thread) `Controller._expand`, which calls
   `nexus.parse_link` for each line (`type`: nexus, github, url, file, collection; `file` only for a path that exists).
   A bad line is logged and skipped.
2. `Controller._add` expands collections and requirements, adds compatibility patches (`_compat`), sends frameworks
   to `add_framework`, and makes jobs with `_job` (state `queued`).
3. A worker thread (`Controller._worker`) takes the job and runs `Controller._run`:
   - Nexus without a premium key: try `nexus.github_copy`; else state `waiting` and the user clicks on Nexus.
     The `nxm://` link comes back through `app.py` -> pipe -> `ui.App._take` -> `Controller.handle_nxm`.
   - `fetch.download` into a new `job-*` folder -> `scan.scan` (if setting `virus_scan`) -> `Controller.install_job`.
4. `install_job` calls `install.install` (unpack, FOMOD, `layout`), writes the mod record into `store.mods`,
   then `_after_job` / `redeploy` calls `deploy.deploy` to copy enabled mods into the game folder.
Job states: `queued`, `running`, `waiting` (free-account click), `needkey`, `done`, `failed`.

### Game launch and crash
1. `watch.watch` (thread started in `Controller.__init__`) calls `Controller._game_started` and `_game_exited`.
2. `_game_exited` gets `watch.crash_evidence` (exit code, Windows events, REDEngine minidump, crash ID `fault`)
   or freeze evidence. After a crash it zips the logs (`doctor.keep_logs`) and adds the crash to
   `store.doctor["crashes"]`. It saves the exit in `store.doctor["last_exit"]`. Then:
   - A bisect is running (`store.doctor["bisect"]`): `_bisect_result(crashed, secs)` and stop.
   - No crash: save `last_good` (and `last_good_game`), add the mods that logged errors to `clean_errors`, and stop.
   - Crash or freeze: `doctor.diagnose` makes a list of problems, and `Controller.handle_problems` handles them.
3. `handle_problems` runs a fix without asking only when all are true: the game crashed, setting `auto_fix` is on,
   the problem has `auto=True`, severity `high`, and exactly one fix. The rest go to the window
   (`notify("problems")` -> `App.show_problems`). `Controller.apply(problem, fix)` runs a fix and logs it in
   `store.doctor["history"]`.
4. Bisect (find the bad mod by halves): `doctor.bisect_start`, `doctor.bisect_step`, `Controller.bisect_begin`.

### Problem and fix shapes (`crash_doctor/doctor.py`, `problem` and `fix`)
- A problem: `{"id", "title", "detail", "fixes", "severity", "auto"}`. Make one with `doctor.problem(...)`.
- A fix: `{"label", "action", "recommended"}`, `action` = `[verb, *args]`. Make one with `doctor.fix(...)`.
- Verbs handled in `Controller.apply`:
  - Can be undone (`Controller.undo`): `disable`, `enable`, `disable_many`, `order`, `prefer`, `ignore`.
  - Cannot be undone (they start work): `update` (`update_mod`), `install_fw` (`add_framework`), `queue_nexus`, `bisect_start`.
  A new verb needs a branch in `Controller.apply`; if it changes the mod list, also an `undo` value and a branch in `undo`.

### Local AI
`Controller.ask_ai` -> `llm.start` -> `Controller.check_now` (runs `doctor.diagnose` and `handle_problems`) ->
`llm.advise` (JSON schema answers: `SCHEMA`, `REPORT_SCHEMA`, `order_schema`) -> `notify("ai")` -> `App.show_ai`
-> `Controller.apply_ai`. The engine stops while the game runs (`llm.block`).
Load-order rules from the AI go into `doctor.best_order(store, after)`.

### Window events (`notify(kind, payload)` -> `App._drain`)
`job` (one download row), `mods` (mod list changed), `log`, `key` (API key check), `game` (running or not),
`problems`, `video`, `browse`, `thumbs`, `ai_status`, `ai`, `pack_choice`, `outdated`, `show`, `confirm` (an nxm link
nobody asked for: the user must say yes).

### Threads and locks
Three download workers, one game-watch thread, and `Controller.bg(fn)` for one-off work. `Controller.lock` guards
the job list; `store.lock` (re-entrant) guards all store data. Code inside `apply` takes `store.lock` again, so keep it an `RLock`.

### AI apps (MCP)
`app.py --mcp` -> `mcp.serve` reads JSON-RPC on stdin -> `app.ask_window` sends the call over the owner-only pipe
-> `ui.App._take` -> `tools.run` -> calls the controller. Tool list: `ai_link/tools.py`, `TOOLS`.

## Saved data (`shared/store.py`, file `state.json` in the data folder)
- Data folder: `%LOCALAPPDATA%\CP2077ModManager`, or the `CP2077MM_DATA` environment variable. Subfolders: `downloads`, `mods` (staging), `backups`.
- `settings`: `game_dir`, `api_key`, `virus_scan`, `auto_fix`, `include_optional`, `theme` (defaults: `DEFAULTS`).
- `mods`: list of records: `id`, `name`, `version`, `enabled`, `source`, `files` (game-relative paths), `needs`
  (framework names), `requires`, `framework`, `installed`. Mod ids (`Controller.install_job`): `nexus-<mod>-<file>`,
  `gh-<owner>-<repo>`, `url-<name>`, `file-<name>`. An update (`replace`) keeps the old id.
- `deployed`: lowercase game path -> mod id (None = copy again). `deploy_game`: the game folder it describes.
  `modlist_tracked`: whether deploy owns `modlist.txt`.
- `doctor`: `last_launch`, `last_exit`, `last_good`, `last_good_game`, `bisect`, `history`, `ignored`,
  `crashes` (last 50: time, crash ID, seconds), `clean_errors` (mods that logged errors in the last 3 clean launches).
- Crash log zips: `crash_logs/` in the data folder (newest 10).
- `packs`: pack name -> `[[mod id, on], ...]`; `pack`: current pack; `pack_info`: Nexus pack links and names.
- Change data only under `store.lock`, then call `store.save()`.

## Where to change things
| Task | File and symbol | Test file |
|---|---|---|
| New link type or Nexus query | `downloads/nexus.py` (`parse_link`, `gql`), then `Controller._add` | `tests/test_links.py` |
| New core framework (from GitHub) | `downloads/fetch.py` (`FRAMEWORKS`) and `controller.CORE` | `tests/test_links.py` |
| Where a kind of mod file installs | `installer/install.py` (`layout`, `ROOTS`, `needs_for`) | `tests/test_install.py` |
| How files reach the game folder | `installer/deploy.py` (`deploy`, `load_order`) | `tests/test_deploy.py` |
| New crash rule | `crash_doctor/doctor.py` (add a check, call it in `diagnose`) | `tests/test_doctor.py` |
| New fix type | `doctor.fix(...)` in the rule + a branch in `Controller.apply` and `undo` | `tests/test_controller.py` |
| Crash or freeze detection | `crash_doctor/watch.py` (`watch`, `crash_evidence`, `read_dump`) | `tests/test_doctor.py`, `tests/e2e_crash.py` |
| Auto-sort order | `doctor.best_order`, `Controller.auto_sort` | `tests/test_doctor.py` |
| Mod packs | `Controller.save_pack`, `use_pack`, `merge_packs`; window: `App._tab_my`, `_pack_card` | `tests/test_controller.py` |
| New setting | `shared/store.py` (`DEFAULTS`) + `App._tab_settings`, `save_settings`, `refresh_settings` | `tests/test_ui.py` |
| New page or button in the window | `manager_ui/ui.py` (`App._tab_*`, `_buttons`), action in `controller.py` | `tests/test_ui.py` |
| Colours and themes | `ui.THEMES`, `App._style`, `App._recolor` | `tests/test_ui.py` |
| What the local AI is asked or how its answer is checked | `local_ai/llm.py` (`RULES`, `SCHEMA`, `advise`) | `tests/test_ai.py`, `tests/ai_bench.py` |
| New tool for AI apps | `ai_link/tools.py` (`TOOLS` + a function, then `run`) | `tests/test_ai.py` |
| Video sites, mods found in a video | `video_finder/finder.py` (`video_text`, `links`, `find`) | `tests/test_video.py` |
| Virus scan | `virus_scan/scan.py` | `tests/test_virus_scan.py` |
| Start-up, `nxm://` links, single window | `app.py` (`main`, `register_nxm`, `HandOver`) | `tests/test_app.py` |

## Files that are not code
- `CP2077 AI Mod Manager.exe`: build output. Never edit it; rebuild it (see `CLAUDE.md`).
- `local_ai/MODEL.md`: model license notes. `local_ai/model/` and `local_ai/engine/` are downloaded, not in git.
- `.github/workflows/tests.yml`: runs all tests on Windows for each push and pull request.

## Index (generated, do not edit by hand)
<!-- BEGIN GENERATED: python tools/code_map.py -->
### `ai_link/mcp.py` (48 lines)
MCP server on stdin/stdout, so AI apps (Claude Desktop, Claude Code, Cursor and others) can use the mod manager.
Uses: ai_link/tools.py
Used by: app.py

- L10 def `reply`: The answer to one JSON-RPC message, or None for a notification.
- L34 def `serve`: call(tool name, arguments) -> text.

### `ai_link/tools.py` (123 lines)
The tools that AI apps (Claude Desktop, Claude Code, Cursor and other MCP apps) can call.
Uses: downloads/fetch.py, downloads/nexus.py, video_finder/finder.py
Used by: ai_link/mcp.py, manager_ui/ui.py

- L11 `TOOLS` (constant)
- L35 def `add_mods`
- L67 def `find_mods_in_video`
- L76 def `status`
- L93 def `sort_load_order`
- L97 def `check_problems`
- L105 def `run`: One call from the MCP server process, JSON {"tool", "args"} -> {"text"} or {"error"}.
- L117 def `private`: The home folder holds the Windows user name: the AI app (often an online service) sees it as ~.

### `app.py` (194 lines)
Cyberpunk 2077 AI Mod Manager.
Uses: ai_link/mcp.py, manager_ui/ui.py, shared/store.py

- L27 `PIPE` (constant)
- L28 `TOKEN` (constant)
- L29 `NXM_KEY` (constant)
- L32 def `nxm_command`
- L40 def `register_nxm`: Make Windows open nxm:// links with this app (current user only, no admin).
- L50 def `nxm_owner`: The name of the program Windows now gives nxm:// links to (such as "Vortex"), or "" when none is set.
- L62 def `nxm_registered`
- L71 class `_SA`
- L75 class `_OwnerOnlyPipe`: PipeListener whose pipe only its owner (the account that started the window) can open.
  - L79 `.__init__`
  - L87 `._new_handle`
- L97 class `OwnerListener`: Listener(address, family="AF_PIPE") on an owner-only pipe.
  - L100 `.__init__`
- L104 class `HandOver`: One attempt to give msg to the running window, on a helper thread so that a pipe holder that never answers cannot block start-up.
  - L108 `.__init__`
  - L113 `.run`
- L122 def `ask_window`: Run one AI-app tool in the window and return its text.
- L153 def `main`

### `crash_doctor/doctor.py` (659 lines)
Find why the game crashed (or will), and list every way to fix each problem.
Uses: downloads/fetch.py
Used by: manager_ui/controller.py

- L22 `LEVEL_OK` (constant)
- L23 `ERR` (constant)
- L24 `PATH` (constant)
- L25 `ARCH` (constant)
- L26 `ACR` (constant)
- L27 `ACR_NEXUS` (constant)
- L28 `CODES` (constant)
- L38 `HAND` (constant)
- L40 `STAMP` (constant)
- L41 `SLOW` (constant)
- L42 `LOGS` (constant)
- L44 `SHARED` (constant)
- L50 def `problem`
- L54 def `fix`
- L58 def `owner_index`: (files, dirs): lowercase game path -> mod id.
- L82 def `owner_of`
- L100 def `owners_in`
- L108 def `log_errors`: {mod id: {"lines": [...], "strong": bool}} from framework logs written since `since`.
- L141 def `mod_fixes`
- L148 def `missing_frameworks`
- L163 def `missing_requirements`
- L176 def `archive_hashes`: Resource hashes inside a .archive (RDAR v12: 40-byte header, index of 56-byte file entries).
- L191 def `overlaps`: (sizes, pairs): how many archive resources and loose game files each enabled mod changes, and how many of them each pair of mods (lower, higher in ...
- L204 def `archive_conflicts`
- L222 def `changed_since_good`
- L234 def `best_order`: Every mod id in the load order most likely to work (index 0 = lowest priority).
- L282 def `moved`: How many mods the order `new` moves compared with `old`: all but the most that keep their order.
- L292 def `acr_report`: (path, text) of the newest Advanced Crash Reporter report written since `since`, or None.
- L312 def `call_path`: Mods whose code is on the stack of the crashing thread (or of the main thread in a freeze).
- L333 def `freeze_problems`: Problems from the snapshot of a game that stopped responding and never came back.
- L350 def `hand_installed`: [(game path, modified time)] of mods in the game folder that this app did not install, newest first.
- L376 def `recent_hand_installed`
- L389 def `game_version`: File version of the game's exe, e.g.
- L404 def `game_updated`: A problem when the game changed since the last launch that worked: frameworks often break after a patch.
- L421 def `slow_mods`: {mod id: seconds} for DLL mods that took longer than `limit` to load, from RED4ext's log: the time from the line that loads the mod to the next line.
- L442 `AI_ASK` (constant)
- L446 def `report`: Plain-text report to share with a mod author, or (for_ai) to paste into an AI chat.
- L485 def `diagnose`: All problems, worst first.
- L553 def `crash_notes`: The crash ID and how often it happened, and where the crash logs were saved.
- L565 def `noisy_mods`: Ids of mods that logged errors in a launch that ended without a crash.
- L571 def `keep_logs`: Zip this crash's REDEngine report and the mod logs written since `since` into the data folder, because the game deletes old logs.
- L604 def `requires_pairs`: (required mod id, mod id that needs it) from the Nexus requirements of `mods`.
- L613 def `bisect_start`: Suspects come after the mods they require, so every first half (`testing`) holds what its mods need.
- L635 def `bisect_off`: Mods to turn off for this launch: the suspects not tested, and every mod that needs one of them, so the game can still start.
- L644 def `bisect_step`: Next state after one launch.

### `crash_doctor/watch.py` (283 lines)
Watch for the game process, and after it exits collect crash evidence: its exit code, Windows crash events, and REDEngine crash reports (whose mini...
Used by: manager_ui/controller.py

- L14 `EXE` (constant)
- L15 `NO_WINDOW` (constant)
- L16 `SYS32` (constant)
- L18 `WAIT_TIMEOUT` (constant)
- L19 `HANG_AFTER` (constant)
- L23 class `SymbolInfo`
- L30 def `kernel32`
- L39 def `hung`: True when a visible window of process `pid` is "Not responding".
- L57 def `write_dump`: Snapshot (threads, their stacks, modules) of running process `pid` as a minidump in `folder`: its path, or None.
- L84 def `find_pid`
- L94 def `watch`: Call on_start(t) when the game starts and on_exit(t_started, exit_code, freeze) when it stops, until stop is set.
- L138 def `parse_events`: [(faulting module path or '' for hangs, process start as unix time or None, fault offset or None)] for game crash events in `wevtutil /f:text` output.
- L152 def `symbol`: Function name at `addr` from the .pdb file next to a module, or None.
- L180 def `read_dump`: From a minidump (.dmp): module: path of the module the crash happened in (None for a freeze snapshot, which has no crash) code: exception code stac...
- L240 def `dump_module`: Path of the module the crash happened in, read from a minidump (.dmp), or None.
- L245 def `fault_id`: Short crash ID, the same for every crash at the same instruction: 'Bad.dll+0x10'.
- L250 def `_mtime`
- L257 def `crash_evidence`: `since` is when polling saw the game start (up to one poll late); events of older game processes are ignored.

### `downloads/fetch.py` (126 lines)
HTTP helpers, file downloads with progress, and GitHub releases for the core frameworks.
Uses: shared/store.py
Used by: ai_link/tools.py, crash_doctor/doctor.py, downloads/nexus.py, local_ai/llm.py, manager_ui/controller.py, manager_ui/ui.py

- L15 `SYS32` (constant)
- L17 `UA` (constant)
- L20 `FRAMEWORKS` (constant)
- L30 `FW_BY_NEXUS` (constant)
- L31 `FW_BY_REPO` (constant)
- L34 def `http_json`
- L41 def `github_latest`
- L50 def `ascii_name`: A plain ASCII file name.
- L58 `SHRINK` (constant)
- L76 def `thumbnails`: {url: png path} of w x h crops of Nexus images, cached in folder.
- L105 def `download`: Download url into folder.

### `downloads/nexus.py` (259 lines)
Nexus Mods: link parsing, mod and collection lookups (GraphQL, no key needed) and download links (v1 REST).
Uses: downloads/fetch.py
Used by: ai_link/tools.py, manager_ui/controller.py, manager_ui/ui.py, video_finder/finder.py

- L15 `GQL` (constant)
- L16 `API` (constant)
- L17 `GONE` (constant)
- L18 `GAME_2_0` (constant)
- L19 `OUTDATED` (constant)
- L23 def `outdated`: Why a mod may not work with the game any more, or "" when nothing says so.
- L34 def `gql`
- L41 def `mod_info`
- L54 def `pick_file`: The asked-for file, else the newest primary MAIN file, else the newest file still listed.
- L62 def `newer_file`: A newer upload that replaces file_id (same file name first), or None when file_id is current.
- L72 def `collection`
- L85 def `mod_names`: {mod id: {"name", "endorsements"}} for Cyberpunk mods, in one request.
- L93 def `mods_requiring`: [{"mod_id", "name"}] of the Cyberpunk mods that list mod_id as a requirement (patches and add-ons for it).
- L107 def `github_copy`: {"repo", "url"} of the author's own GitHub release of a Nexus mod, so a free account needs no click; else None.
- L134 def `collection_info`: {"name", "endorsements"} of a Cyberpunk collection, without its file list.
- L140 def `search`: Cyberpunk mods with any of `words` in their name, best first.
- L157 `SORTS` (constant)
- L161 def `browse`: One page of Cyberpunk mods for the Browse tab: {"total", "categories": {name: count}, "mods": [...]}.
- L183 def `browse_packs`: One page of Cyberpunk collections (mod packs), in the same shape as browse().
- L207 def `validate`
- L211 def `download_url`
- L217 def `files_page`: The page where a free user clicks "Slow download"; Nexus then hands the nxm:// link to this app.
- L222 def `parse_link`: One pasted line -> a job description dict, None for blank/comment lines, ValueError when unknown.

### `installer/deploy.py` (114 lines)
Copy enabled mods from staging into the game folder.
Uses: shared/store.py
Used by: manager_ui/controller.py

- L10 `ARCHIVES` (constant)
- L12 `KEEP` (constant)
- L16 def `plan`
- L25 def `_remove`
- L38 def `_switch_game`: The game folder changed since the last deploy: take the mods out of the old folder first.
- L53 def `load_order`: (managed, others): archive file names in load order.
- L66 def `deploy`: Sync the game folder with the enabled mods.

### `installer/install.py` (213 lines)
Turn a downloaded archive into game-relative files: unpack, run a FOMOD installer with its default choices, and put loose files in the folder the g...
Used by: manager_ui/controller.py

- L13 `TAR` (constant)
- L14 `NO_WINDOW` (constant)
- L15 `ROOTS` (constant)
- L16 `CET_MODS` (constant)
- L19 def `extract`: Unpack `archive` into the new folder `dest` with tar.exe.
- L39 def `safe`
- L43 def `_inside`: A relative path that stays inside its folder.
- L50 def `fomod_tree`: If the mod has a FOMOD installer, copy the default choices into `out` and return it, else None.
- L115 def `layout`: [(source file, game-relative posix path)] for everything the game can use in `tree`.
- L168 def `needs_for`: Framework names the files need, from where they install and what kind they are.
- L187 def `install`: Unpack `archive` into the staging folder `stage` in game layout.

### `local_ai/llm.py` (211 lines)
The local AI: an open model (Qwen3.5, Apache 2.0 license) that runs on this PC in llama.cpp's llama-server (MIT license).
Uses: downloads/fetch.py, shared/store.py
Used by: manager_ui/controller.py, manager_ui/ui.py

- L23 `HOME` (constant)
- L24 `ENGINE` (constant)
- L26 `MODEL` (constant)
- L29 `SERVER` (constant)
- L30 `MODEL_FILE` (constant)
- L33 `BLOCKED` (constant)
- L34 `RULES` (constant)
- L42 `SCHEMA` (constant)
- L46 `REPORT` (constant)
- L51 `ACTIONS` (constant)
- L52 `REPORT_SCHEMA` (constant)
- L56 `ORDER` (constant)
- L62 def `order_schema`: Asks about each of the n mods by number: small models leave a free list of pairs empty, but fill one slot per mod.
- L69 def `ready`
- L73 def `fetch`: For a run from source (the release zip has both files): download the engine and the model and check their hashes.
- L89 def `checked`
- L100 def `running`
- L104 def `block`: on: the game starts or the window closes.
- L113 def `start`: Start the engine with the model, on a free local port, and wait until the model is loaded.
- L151 def `stop`: Give the PC back (the game starts, or the window closes): end the engine, which frees the model's memory.
- L161 def `chat`
- L173 def `advise`: {"summary", "fixes": [(problem, fix, why)], "after": [(lo id, hi id)]}: hi must load after (win over) lo.

### `manager_ui/controller.py` (803 lines)
Everything the window does, without Tk: links -> jobs -> download -> virus scan -> install -> deploy, plus game launch, crash handling, fixes and u...
Uses: crash_doctor/doctor.py, crash_doctor/watch.py, downloads/fetch.py, downloads/nexus.py, installer/deploy.py, installer/install.py, local_ai/llm.py, shared/store.py, video_finder/finder.py, virus_scan/scan.py
Used by: manager_ui/ui.py

- L27 `COMPAT` (constant)
- L28 `NOT_COMPAT` (constant)
- L29 `CORE` (constant)
- L30 `GAME_ON` (constant)
- L33 class `Controller`
  - L34 `.__init__`
  - L50 `.key`
  - L53 `.bg`
  - L61 `.check_key`
  - L79 `.add_links`
  - L82 `.add_link`: One link that is parsed already, such as the nxm link the user said yes to.
  - L86 `._expand`
  - L95 `._have`
  - L99 `._job`
  - L107 `._set`
  - L111 `._add`
  - L165 `._compat`: Add the compatibility patches for mod_id and the user's other mods.
  - L194 `.add_framework`
  - L205 `.handle_nxm`: An nxm:// link from the browser.
  - L235 `._next_click`: Free accounts: open the Nexus page of the next waiting file, one at a time.
  - L245 `.reopen`
  - L248 `.skip`: Only a download that has not started can be skipped.
  - L258 `.retry`
  - L267 `._worker`
  - L289 `._run`
  - L348 `.install_job`
  - L379 `._after_job`
  - L387 `.redeploy`
  - L398 `._try`
  - L404 `.set_enabled`
  - L412 `.move`
  - L422 `.save_pack`: Keep the mod list as it is now (which mods are on, and their order) as the mod pack `name`.
  - L433 `.use_pack`: Turn on the pack's mods in the pack's order and turn off the rest.
  - L450 `.merge_packs`: One pack with every mod of `names`.
  - L469 `.delete_pack`
  - L478 `.remove`
  - L485 `.update_mod`
  - L500 `.has_update`
  - L512 `.launch`: Start the game with the mods that are on, or first set the mod list to the mod pack `pack`.
  - L544 `._game_started`
  - L555 `._game_exited`
  - L591 `.crash_report`: Text of a report about the last game exit and the current mods, to share with a mod author or an AI chat.
  - L596 `.check_now`
  - L602 `.handle_problems`
  - L612 `.apply`
  - L647 `.undo`
  - L667 `._order`: Sort the mod list by `ids`; a mod not in `ids` (added since) goes last, in its old order.
  - L672 `.auto_sort`: Put the mods in the best load order (the Crash doctor can undo it), then check them for problems.
  - L685 `.setup_ai`: Download the engine and the model when they are missing (a run from source), then start the engine once.
  - L699 `.ask_ai`: Check the mods, let the local AI pick fixes and load-order rules, and send the advice to the window.
  - L731 `.apply_ai`: The order first, so that a fix that moves one mod (prefer) keeps the last word.
  - L740 `.find_in_video`
  - L748 `.bisect_begin`
  - L758 `._bisect_apply`
  - L769 `.bisect_cancel`
  - L776 `._bisect_result`

### `manager_ui/ui.py` (1943 lines)
The window.
Uses: ai_link/tools.py, downloads/fetch.py, downloads/nexus.py, local_ai/llm.py, manager_ui/controller.py, shared/store.py
Used by: app.py

- L23 `THEMES` (constant)
- L31 `BG, PANEL, PANEL2, FG, DIM, ACCENT, HOT, INK, HOVER, SEL, BANNER` (constant)
- L34 def `use_theme`
- L40 `KEY_PAGE` (constant)
- L41 `EXAMPLES` (constant)
- L44 `LEVEL` (constant)
- L45 `ACTIVE` (constant)
- L46 `CURRENT` (constant)
- L47 `ICONS` (constant)
- L49 `RAIL` (constant)
- L53 class `Tip`: Help text that shows while the mouse is over a widget.
  - L56 `.__init__`
  - L62 `.show`
  - L70 `.hide`
- L76 def `paint`: Draw shape(x, y) -> colour or None into img, smoothed with 3x3 samples per pixel.
- L97 def `pill`: True inside a w x h stadium (a rectangle with round ends).
- L103 def `switch`: A toggle switch picture for the mod list: an accent pill with the knob right when on, a grey one when off.
- L112 def `mark`: A check box ("check") or option button ("radio") picture in the theme colours.
- L139 def `short`: 20381596 -> '20.4M', like a store page.
- L147 def `ago`: '2026-11-17T10:00:00Z' -> '3 days ago'.
- L160 class `App`
  - L161 `.__init__`
  - L215 `.px`
  - L219 `._style`
  - L318 `._buttons`: spec: (label, command, tooltip).
  - L328 `._tree`
  - L343 `._text`
  - L347 `._header`
  - L374 `._rail`: The page bar on the left, like a launcher: an icon per page, its name shows on hover.
  - L389 `._page_changed`
  - L396 `._statusbar`
  - L405 `._scroller`: (outer frame, canvas, inner frame): a frame that scrolls with the mouse wheel.
  - L420 `._wheel`
  - L427 `._tab_browse`
  - L482 `._browse_shown`
  - L486 `._browse_kind`: Mods <-> Mod packs: each has its own categories, so the list is built again on the next load.
  - L494 `._browse_soon`: Search while the user types, once they pause.
  - L500 `.browse`
  - L517 `.show_browse`
  - L555 `._browse_card`
  - L611 `.show_thumbs`
  - L623 `._browse_marks`: Install buttons say 'Installed' for mods that are in the mod list.
  - L634 `.browse_install`
  - L647 `._tab_mods`
  - L735 `._tab_my`: My mod packs: the Nexus packs the user installed and the packs they saved or merged, as cards.
  - L760 `.find_packs`
  - L766 `.refresh_pack_page`
  - L778 `._pack_card`
  - L819 `.launch_pack`
  - L824 `._search_box`: A search field with a magnifier and a grey hint while it is empty.
  - L838 `._pack_head`
  - L846 `._tab_add`
  - L876 `._tab_find`
  - L899 `._tab_downloads`
  - L919 `._tab_crash`
  - L964 `._tab_ai`
  - L1005 `._tab_settings`
  - L1061 `._selected`
  - L1064 `._safe`
  - L1070 `.say`
  - L1073 `.select_all`
  - L1077 `.close`: No new copy starts.
  - L1091 `._close_when_free`
  - L1099 `.redeploy`
  - L1107 `.launch`
  - L1120 `.picked_pack`
  - L1124 `.refresh_packs`
  - L1130 `.save_pack`
  - L1143 `.use_pack`
  - L1150 `.delete_pack`
  - L1156 `.merge_packs`
  - L1190 `._mods_hover`: Light up the row under the mouse.
  - L1201 `._mods_click`: The switch turns a mod on or off; the Actions cell holds update, remove and more.
  - L1217 `.add_dialog`: Ways to add mods, as a list to pick from.
  - L1260 `.pack_choice`: A mod pack with optional mods, or with mods that may not work with the game any more: the user picks what to download.
  - L1302 `.ask_outdated`: A mod that may not work with the game any more: Download it anyway, or Skip it.
  - L1323 `._hot`
  - L1327 `._mods_menu`
  - L1334 `._set_on`
  - L1338 `.toggle`
  - L1344 `.move`
  - L1350 `.auto_sort`
  - L1354 `.update`
  - L1360 `.remove`
  - L1370 `.open_folder`
  - L1375 `.install_core`
  - L1381 `._placeholder`
  - L1389 `._links_text`
  - L1392 `._count_links`
  - L1396 `.paste`
  - L1405 `.add_links`
  - L1416 `.add_files`
  - L1424 `.find_video`
  - L1431 `.show_found`
  - L1444 `.add_found`
  - L1452 `.open_found`
  - L1456 `.setup_ai`
  - L1464 `.ask_ai`
  - L1468 `.ask_bug`
  - L1475 `.show_ai`
  - L1494 `.copy_setup`: The MCP setup that starts this app as `--mcp` (the exe, or python with app.py when run from source).
  - L1504 `._job_do`
  - L1514 `.retry_failed`
  - L1522 `.clear_jobs`
  - L1530 `.show_problem`
  - L1551 `.apply_fix`
  - L1566 `.undo`
  - L1573 `.save_report`
  - L1584 `.copy_report`
  - L1589 `.open_crash_logs`
  - L1595 `.bisect_begin`
  - L1601 `.bisect_cancel`
  - L1605 `.browse_game`
  - L1611 `.detect_game`
  - L1619 `.save_settings`
  - L1631 `.set_theme`
  - L1641 `._recolor`: Plain Tk widgets keep the colours they were made with; ttk ones follow _style.
  - L1659 `.check_key`
  - L1667 `.register_nxm`
  - L1676 `.hide_banner`
  - L1681 `.refresh_settings`
  - L1696 `.refresh_setup`
  - L1705 `.refresh_mods`
  - L1738 `.show_job`
  - L1748 `.refresh_progress`
  - L1781 `.refresh_tabs`
  - L1793 `.refresh_history`
  - L1799 `.refresh_bisect`
  - L1808 `.show_problems`
  - L1828 `.log_line`
  - L1835 `._drain`
  - L1882 `._serve`: Second app instances (started by nxm:// links) connect to the named pipe here.
  - L1894 `._take`: Both sides prove they know the token; only then is the nxm link (or "show") read.
- L1934 def `run`

### `shared/store.py` (99 lines)
App data folder, settings and the installed-mod list, kept in one JSON file.
Used by: app.py, downloads/fetch.py, installer/deploy.py, local_ai/llm.py, manager_ui/controller.py, manager_ui/ui.py

- L9 `VERSION` (constant)
- L10 `DATA` (constant)
- L11 `GAME_EXE` (constant)
- L12 `DEFAULTS` (constant)
- L15 def `kind_of`: A mod's file types for people: "Framework", or e.g.
- L29 class `Store`: mods[0] is the lowest priority; the last mod in the list wins every conflict.
  - L32 `.__init__`
  - L51 `.save`
  - L60 `.mod`
  - L63 `.enabled`
  - L66 `.game`
- L71 def `find_game`: Look in Steam libraries, GOG and Epic for the Cyberpunk 2077 folder.

### `tests/ai_bench.py` (130 lines)
Bug-chat benchmark with the real local AI: `py -u tests/ai_bench.py practice|hidden [model.gguf]`.
Uses: local_ai/llm.py, manager_ui/controller.py, shared/store.py, tests/helpers.py

- L19 `NAMES` (constant)
- L37 `WORDS` (constant)
- L53 `FW_FILES` (constant)
- L56 def `frameworks`
- L61 def `cases`
- L82 def `run_case`
- L109 def `main`

### `tests/e2e_crash.py` (98 lines)
End-to-end crash check with a REAL crash (Windows only, about 1 minute, not part of the unit suite).
Uses: manager_ui/controller.py, shared/store.py, tests/helpers.py

- L24 `PY` (constant)
- L52 def `play`: Start the 'game' like a launcher would; the manager only watches.

### `tests/fake_game.py` (45 lines)
Stand-in for Cyberpunk2077.exe in tests/e2e_crash.py.

- L25 class `ExceptionInfo`
- L31 def `reporter`

### `tests/helpers.py` (43 lines)
-

- L6 def `make_zip`: files: {"inner/path": "text" or bytes}
- L14 def `rdar`: Bytes of a minimal .archive holding the given resource hashes.
- L21 def `fake_game`
- L29 def `mod`
- L35 def `stage`: Write staging files for mod m and add it to the store.

### `tests/test_ai.py` (187 lines)
The AI-app link (MCP server and its tools) and the local AI's answer checks, with no real AI app or model.
Uses: ai_link/mcp.py, ai_link/tools.py, crash_doctor/doctor.py, local_ai/llm.py, shared/store.py, tests/helpers.py

- L19 def `rpc`
- L23 class `Mcp`
- L47 class `Tools`
- L89 class `LocalAi`

### `tests/test_app.py` (151 lines)
Start-up hand-over in app.main, on a private test pipe; the window itself is replaced by a stub.
Uses: app.py, manager_ui/ui.py

- L16 `LINK` (constant)
- L19 class `StartUp`

### `tests/test_code_map.py` (28 lines)
tools/code_map.py: the generated index of CODE_MAP.md.

- L6 `ROOT` (constant)
- L12 class `Index`

### `tests/test_controller.py` (417 lines)
-
Uses: manager_ui/controller.py, shared/store.py, tests/helpers.py

- L17 def `wait_idle`
- L28 class `Pipeline`

### `tests/test_deploy.py` (217 lines)
-
Uses: installer/deploy.py, shared/store.py, tests/helpers.py

- L11 class `Deploy`

### `tests/test_doctor.py` (426 lines)
-
Uses: crash_doctor/doctor.py, crash_doctor/watch.py, shared/store.py, tests/helpers.py

- L19 `EVENTS` (constant)
- L35 def `minidump`: A small minidump: exception (none when crash_addr is None, like a freeze snapshot), module list, and thread `tid` whose stack holds stack_words, th...
- L66 class `Doctor`

### `tests/test_install.py` (174 lines)
-
Uses: installer/install.py, tests/helpers.py

- L11 `FOMOD` (constant)
- L34 class `Install`

### `tests/test_links.py` (126 lines)
-
Uses: downloads/fetch.py, downloads/nexus.py, manager_ui/ui.py

- L10 class `FileNames`
- L18 class `Outdated`
- L27 class `GithubCopy`
- L46 class `Links`
- L93 class `Browse`

### `tests/test_ui.py` (474 lines)
Builds the real window (hidden) on a throwaway store and drives its actions.
Uses: manager_ui/ui.py, shared/store.py, tests/helpers.py

- L20 class `Window`

### `tests/test_video.py` (156 lines)
Find mod from video, offline: pages and Nexus answers are canned.
Uses: downloads/nexus.py, manager_ui/controller.py, video_finder/finder.py

- L12 `NAMES` (constant)
- L13 `FOUND` (constant)
- L18 def `fake_gql`
- L28 def `youtube`
- L32 class `Finder`
- L124 def `resolves_to`
- L128 class `PublicPagesOnly`

### `tests/test_virus_scan.py` (33 lines)
Microsoft Defender result handling.
Uses: virus_scan/scan.py

- L9 `FOUND` (constant)
- L15 class `Scan`

### `video_finder/finder.py` (186 lines)
Find the Cyberpunk 2077 mods a video shows, from the text around it: mod links in the description first, else a Nexus name search with the title, t...
Uses: downloads/nexus.py
Used by: ai_link/tools.py, manager_ui/controller.py

- L16 `HEADERS` (constant)
- L17 `LINK` (constant)
- L18 `CHAPTER` (constant)
- L19 `NOISE` (constant)
- L25 def `public`: Refuse a link to this PC or the local network (a router page, another app's local server): the link comes from the user, an AI app or a redirect, a...
- L37 class `_PublicRedirects`
  - L38 `.redirect_request`
- L43 `OPENER` (constant)
- L46 def `page`: The page at url as text, and the address it ended at after redirects.
- L55 def `youtube_id`
- L67 def `_json_after`: The JSON value that starts after `marker` in text, or None.
- L78 def `_meta`: {name: content} of a page's <meta> tags (og:title, description, ...).
- L87 def `video_text`: (title, description or caption) of the video at url.
- L119 def `links`: Mods linked in the text: Nexus mods and collections for Cyberpunk, and GitHub repositories.
- L152 def `phrases`: Word lists worth a Nexus name search: the title, then the chapter titles or else the first lines of the text.
- L166 def `find`: (video title, [{"name", "url", "why", "endorsements"}]) for a video link, or for pasted text such as a caption or a mod name.

### `virus_scan/scan.py` (32 lines)
Scan a downloaded file with Microsoft Defender (built into Windows, no admin needed).
Used by: manager_ui/controller.py

- L6 `NO_WINDOW` (constant)
- L9 def `defender`
- L15 def `scan`: (True, '') clean, (False, detail) threat found, (None, detail) could not scan.
<!-- END GENERATED -->
