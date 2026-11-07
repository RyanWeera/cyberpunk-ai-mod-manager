"""Scan a downloaded file with Microsoft Defender (built into Windows, no admin needed)."""
import os
import subprocess
from pathlib import Path

NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def defender():
    plat = sorted(Path(os.environ.get("ProgramData", r"C:\ProgramData"), "Microsoft/Windows Defender/Platform").glob("*/MpCmdRun.exe"))
    exe = Path(os.environ.get("ProgramFiles", r"C:\Program Files"), "Windows Defender/MpCmdRun.exe")
    return plat[-1] if plat else exe if exe.exists() else None


def scan(path):
    """(True, '') clean, (False, detail) threat found, (None, detail) could not scan. Defender looks inside zip/7z/rar."""
    exe = defender()
    if not exe:
        return None, "Microsoft Defender is not available on this PC"
    try:
        r = subprocess.run([str(exe), "-Scan", "-ScanType", "3", "-File", str(path), "-DisableRemediation"],
                           capture_output=True, text=True, errors="replace", timeout=1800, creationflags=NO_WINDOW)
    except (OSError, subprocess.TimeoutExpired) as e:
        return None, str(e)
    if r.returncode == 0:
        return True, ""
    lines = [ln.strip() for ln in r.stdout.splitlines() if ln.strip()]
    if r.returncode == 2:
        return False, "; ".join(ln for ln in lines if "Threat" in ln or "threat" in ln)[:300] or "Threat found"
    return None, (lines[-1] if lines else f"Defender exit code {r.returncode}")
