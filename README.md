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
- **Automatic set-up.** Every archive is unpacked and each file goes to the folder the game loads it from (`archive/pc/mod`, `r6/scripts`, `r6/tweaks`, `red4ext/plugins`, Cyber Engine Tweaks mods, REDmod). FOMOD installers run with their default choices. Missing frameworks are installed by themselves.
- **No-click frameworks.** RED4ext, Cyber Engine Tweaks, redscript, ArchiveXL, TweakXL, Codeware, Input Loader and Mod Settings download from their GitHub releases, with no account.
- **Crash doctor.** The app watches the game. After a crash it reads the exit code, the game's crash dump, Windows crash events and the framework logs (redscript, RED4ext, ArchiveXL, TweakXL, CET) to find the mod that caused it.
  - One possible fix: it is applied automatically (you can turn this off).
  - More than one fix (update, turn off, change the load order): you pick.
  - Every fix can be undone.
  - No clue in the logs: "Find the bad mod for me" turns half of the mods off per game start until the bad one is found.
- **Load order and conflicts.** Mods lower in the list win. Archive conflicts are found by reading the `.archive` file indexes, and you choose which mod wins.
- **Virus scan.** Every download is scanned with Microsoft Defender before it is installed. Turn it off in Settings to go faster with big mod lists.
- **Safe to undo.** Mods stay in their own folder and are copied into the game. Game files that a mod replaces are backed up and put back when the mod is removed.

## Nexus Mods and free accounts

Nexus Mods gives a free account a download only after a person clicks **Slow download** on the website. This app follows the Nexus rules, so with a free account:

1. The app opens the Nexus page of each file in your browser, one at a time.
2. You click **Slow download**.
3. The app downloads, scans, installs and sets up the file, then opens the next page.

With a Nexus Premium account no clicks are needed. Frameworks, GitHub mods and direct links never need a click.

## Download, check and remove

- **Every version:** the [Releases](../../releases) page.
- **Check the file (optional):** each release lists the file's SHA-256. In PowerShell, run `Get-FileHash .\CP2077.AI.Mod.Manager.exe` and compare.
- **Update:** download the newest exe and replace the old one. Your mods and settings stay. If the new exe is in a different folder or has a different name, click **Use this app for Nexus downloads** again. The version is shown in the title bar.
- **Remove:**
  1. In the **Mods** list press **Ctrl+A**, then click **Turn on / off** until every row shows ○. This takes the mods' files out of the game folder and puts back the game files they replaced.
  2. Wait until the status bar says the mods are set up in the game folder, then close the app.
  3. Delete the exe and the `%LOCALAPPDATA%\CP2077ModManager` folder.
  4. If you use another mod manager (Vortex, Mod Organizer 2), set it up for Nexus downloads again, so the Nexus download buttons open it.

Requirements: Windows 11, or Windows 10 (the app uses the built-in `tar.exe` and Microsoft Defender). The `tar.exe` in Windows 10 is older and cannot unpack some `.7z` and `.rar` mods; the app then says so.

### Run from source

```
python app.py
```

Python 3.12 or newer, standard library only.

## Where your data is

Mods, downloads, backups and settings (including your Nexus API key) are kept in `%LOCALAPPDATA%\CP2077ModManager` on your PC. The key is sent only to `api.nexusmods.com`.

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
