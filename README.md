# Cyberpunk 2077 AI Mod Manager

A Windows mod manager for Cyberpunk 2077 that sets your mods up for you and fixes crashes.

<p align="center">
  <a href="https://github.com/RyanWeera/cyberpunk-ai-mod-manager/releases/latest/download/CP2077.AI.Mod.Manager.exe"><img src="https://img.shields.io/badge/Download_for_Windows-.exe-fcee0a?style=for-the-badge&logo=windows&logoColor=black&labelColor=fcee0a&color=0e1016" alt="Download for Windows"></a>
</p>
<p align="center">
  <a href="https://github.com/RyanWeera/cyberpunk-ai-mod-manager/releases/latest"><img src="https://img.shields.io/github/v/release/RyanWeera/cyberpunk-ai-mod-manager?label=latest" alt="Latest version"></a>
  <img src="https://img.shields.io/github/downloads/RyanWeera/cyberpunk-ai-mod-manager/total" alt="Downloads">
  <img src="https://img.shields.io/github/license/RyanWeera/cyberpunk-ai-mod-manager" alt="License: MIT">
</p>

**One file. No installer. No Python.** Windows 11 (Windows 10 works for most mods). The button always gives you the newest version.

![Mods tab](docs/screenshot.png)

## Get started in 3 steps

1. **Download and open it.** Click the yellow button above, then double-click `CP2077.AI.Mod.Manager.exe`. You can keep it anywhere, for example on your Desktop.
   - If Windows shows "Windows protected your PC", click **More info**, then **Run anyway**. Windows shows this for new apps that are not code-signed yet.
2. **Set it up once (Settings tab).**
   - Click **Find it for me** to find your Cyberpunk 2077 folder (Steam, GOG or Epic).
   - Click **Get my key**, copy your personal API key from Nexus, and paste it. Free accounts have one too.
   - Click **Use this app for Nexus downloads**, so the download buttons on Nexus send files to the app.
3. **Add mods and play.** In **Add mods**, paste mod links or a collection link and click **Download and set up everything**. When the **Downloads** tab is finished, click **Launch game**. With a free Nexus account you click **Slow download** once per Nexus file (see [below](#nexus-mods-and-free-accounts)).

The yellow banner at the top of the app shows which set-up steps are still missing.

## What it does

- **Paste links, get mods.** Paste many links at once: Nexus mod pages, Nexus collections (mod packs), GitHub pages, or direct `.zip` / `.7z` / `.rar` links. Each mod's required mods are added for you.
- **Find mod from video.** Paste a YouTube, TikTok, Instagram or other video link. The app reads the video's description or caption and lists the mods it links or names, and you add them with one click. YouTube works best, because its descriptions have the mod links. The app does not watch the video, so a mod that is only shown on screen is not found.
- **Automatic set-up.** Every archive is unpacked and each file goes to the folder the game loads it from (`archive/pc/mod`, `r6/scripts`, `r6/tweaks`, `red4ext/plugins`, Cyber Engine Tweaks mods, REDmod). FOMOD installers run with their default choices. Missing frameworks are installed by themselves.
- **No-click frameworks.** RED4ext, Cyber Engine Tweaks, redscript, ArchiveXL, TweakXL, Codeware, Input Loader and Mod Settings download from their GitHub releases, with no account.
- **Crash doctor.** The app watches the game. After a crash it reads the exit code, the game's crash dump, Windows crash events and the framework logs (redscript, RED4ext, ArchiveXL, TweakXL, CET) to find the mod that caused it.
  - One possible fix: it is applied automatically (you can turn this off).
  - More than one fix (update, turn off, change the load order): you pick.
  - Every fix can be undone.
  - No clue in the logs: "Find the bad mod for me" turns half of the mods off per game start until the bad one is found.
- **Load order and conflicts.** Mods lower in the list win. Archive conflicts are found by reading the `.archive` file indexes, and you choose which mod wins. **Auto-sort** (Mods tab) sorts the list for you: frameworks first, each mod below the mods it needs, and a small patch below the big mod it changes. You can undo it in the Crash doctor tab.
- **Use Claude or another AI app.** Give a mod link or a video link to Claude and ask it to set the mods up. The app is an MCP server: Claude Desktop, Claude Code, Cursor and other MCP apps can add Nexus Mods mods and the core frameworks, sort the load order and check for problems through the app's window. Other GitHub links and direct download links you paste yourself. The **AI helpers** tab copies the setup for you. With a free Nexus account you still click "Slow download" for each Nexus file.
- **Local AI (optional).** The **AI helpers** tab can download a free AI model that runs on your PC with [Ollama](https://ollama.com) (about 3 GB, no account, offline after the download). **Ask the local AI** (Crash doctor tab) lets it read your mods and problems, pick fixes and find patches that must load after the mod they change. You see each change first, and you can undo it. It is a general AI model that gets the app's modding rules; it was not trained only for Cyberpunk.
- **Virus scan.** Every download is scanned with Microsoft Defender before it is installed. Turn it off in Settings to go faster with big mod lists.
- **Safe to undo.** Mods stay in their own folder and are copied into the game. Game files that a mod replaces are backed up and put back when the mod is removed.

## Nexus Mods and free accounts

Nexus Mods gives a free account a download only after a person clicks **Slow download** on the website. This app follows the Nexus rules, so with a free account:

1. The app opens the Nexus page of each file in your browser, one at a time.
2. You click **Slow download**.
3. The app downloads, scans, installs and sets up the file, then opens the next page.

With a Nexus Premium account no clicks are needed. Frameworks, GitHub mods and direct links never need a click.

If you click a "Mod Manager Download" button on Nexus yourself (not on a page the app opened), the app shows the mod's name and asks you before it installs anything. Any web page can send such a link, so this check keeps pages from installing mods without you.

## Download, check and remove

- **Every version:** the [Releases](../../releases) page.
- **Check the file (optional):** each release lists the file's SHA-256. In PowerShell, run `Get-FileHash .\CP2077.AI.Mod.Manager.exe` and compare.
- **Update:** close the app, download the newest exe and replace the old one. Your mods and settings stay. If the new exe is in a different folder or has a different name, click **Use this app for Nexus downloads** again, and copy the AI app setup again (**AI helpers** tab). The version is shown in the title bar.
- **Remove:**
  1. In the **Mods** tab, clear the **Search** box, click any mod, press **Ctrl+A**, then click **Turn on / off** until every row shows ○. This takes the mods' files out of the game folder and puts back the game files (and your own `modlist.txt`) that they replaced.
  2. Wait until the status bar says the mods are set up in the game folder, then close the app.
  3. Delete the exe and the `%LOCALAPPDATA%\CP2077ModManager` folder.
  4. Nexus download buttons still point to the deleted app. If you use another mod manager (Vortex, Mod Organizer 2), set it up for Nexus downloads again. If not, run `reg delete HKCU\Software\Classes\nxm /f` in a Command Prompt.
  5. If you set up an AI app, remove `cp2077-ai-mod-manager` from it. In Claude Code, run `claude mcp remove --scope user cp2077-ai-mod-manager`. In Claude Desktop, delete its entry in **Settings > Developer > Edit Config**.
  6. If you downloaded the local AI, uninstall **Ollama** in Windows **Settings > Apps**. Then delete the `%USERPROFILE%\.ollama` folder if it is still there (it holds the AI model).

Requirements: Windows 11, or Windows 10 (the app uses the built-in `tar.exe` and Microsoft Defender). The `tar.exe` in Windows 10 is older and cannot unpack some `.7z` and `.rar` mods; the app then says so.

### Run from source

```
python app.py
```

Python 3.12 or newer, standard library only.

## Where your data is

Mods, backups and settings (including your Nexus API key) are kept in `%LOCALAPPDATA%\CP2077ModManager` on your PC. Downloaded archives are deleted after the install. The key is sent only to `api.nexusmods.com`.

## Development

```
python -m unittest discover -s tests -t .     # unit tests
python -u tests/e2e_crash.py                  # real-crash end-to-end check (about 1 minute)
```

Build the exe:

```
python -m PyInstaller --noconfirm --clean --windowed --onefile --name "CP2077 AI Mod Manager" --icon manager_ui/icon.ico --add-data "manager_ui/icon.png;manager_ui" --paths . app.py
```

## Disclaimer

This is a fan project. It is not made by or affiliated with CD PROJEKT RED or Nexus Mods. Cyberpunk 2077 is a trademark of CD PROJEKT S.A.

Author: [RyanWeera](https://github.com/RyanWeera)
