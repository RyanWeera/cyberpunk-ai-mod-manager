# Directory Map: CyperPunk Ai Mod Manger
Start here: `manager_ui/` (the root `app.py` only handles the single window and nxm:// links)

## Folders
- `manager_ui/` - window (Tk) and the controller that runs the whole download, install and crash pipeline
- `downloads/` - link parsing, Nexus API (GraphQL and v1), GitHub releases, file download
- `installer/` - unpack, FOMOD defaults, map files to game folders, deploy into the game folder
- `crash_doctor/` - game process watch, crash evidence, log diagnosis, archive conflicts, bad-mod search
- `virus_scan/` - Microsoft Defender scan of downloads
- `shared/` - settings, mod list storage, game folder detection
- `tests/` - unittest suite with fake game folders, plus a real-crash end-to-end check
- `docs/` - README screenshot

## Where to change things
- Add a link type or Nexus query -> `downloads/`
- Change where a kind of mod file installs -> `installer/`
- Add a crash rule or a fix type -> `crash_doctor/` (fix actions run in `manager_ui/`)
- Add a setting -> `shared/` (default) and `manager_ui/` (checkbox)
- Add a framework that installs from GitHub -> `downloads/`

## Ignore when searching
- `graft/` - code graph cache
- `__pycache__/` - Python bytecode
