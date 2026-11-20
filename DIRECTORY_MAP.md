# Directory Map: CyperPunk Ai Mod Manger
Start here: `manager_ui/` (the root `app.py` only handles the single window and nxm:// links). AI editors: read `CODE_MAP.md` first (rules, flows, every function with its line number)

## Folders
- `manager_ui/` - window (Tk) and the controller that runs the whole download, install and crash pipeline
- `downloads/` - link parsing, Nexus API (GraphQL and v1), GitHub releases, file download, hidden-browser auto-click for free Nexus accounts
- `installer/` - unpack, FOMOD defaults, map files to game folders, deploy into the game folder
- `crash_doctor/` - game process watch, crash evidence, log diagnosis, archive conflicts, bad-mod search
- `video_finder/` - find the mods a YouTube, TikTok, Instagram or other video shows, from its description or caption
- `ai_link/` - MCP server, so Claude and other AI apps can add mods and sort or check them through the window
- `local_ai/` - the local AI (bundled llama.cpp engine and model) that explains bugs, picks fixes and finds patches for the load order
- `virus_scan/` - Microsoft Defender scan of downloads
- `shared/` - settings, mod list storage, game folder detection
- `tests/` - unittest suite with fake game folders, plus a real-crash end-to-end check
- `docs/` - README screenshot
- `tools/` - developer scripts: `CODE_MAP.md` generator
- `rust-app/` - Rust port of the whole app (Cargo workspace, one crate per feature folder, egui window); plan and differences in its own notes
- `.github/` - CI workflows: tests on Windows, and the release exe build (on a `v*` tag)

## Where to change things
- Add a link type or Nexus query -> `downloads/`
- Change where a kind of mod file installs -> `installer/`
- Add a crash rule or a fix type -> `crash_doctor/` (fix actions run in `manager_ui/`)
- Change how Auto-sort orders mods -> `crash_doctor/`
- Add a setting -> `shared/` (default) and `manager_ui/` (checkbox)
- Add a framework that installs from GitHub -> `downloads/`
- Read another video site, or change how mods are found in a video -> `video_finder/`
- Add or change a tool that AI apps can call -> `ai_link/`
- Change what the local AI is asked, or how its answer is checked -> `local_ai/`

## Ignore when searching
- `graft/` - code graph cache
- `__pycache__/` - Python bytecode
- `.ruff_cache/` - linter cache
- `rust-app/target/` - Rust build output
