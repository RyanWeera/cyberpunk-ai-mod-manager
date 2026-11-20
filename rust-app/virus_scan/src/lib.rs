//! Scan a downloaded file with Microsoft Defender (built into Windows, no admin needed).
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const NO_WINDOW: u32 = 0x0800_0000;
const TIMEOUT: Duration = Duration::from_secs(1800);

fn env_dir(name: &str, default: &str) -> PathBuf {
    std::env::var_os(name).map(PathBuf::from).unwrap_or_else(|| default.into())
}

/// Full path of `MpCmdRun.exe`: the newest platform folder, else the one in Program Files.
pub fn defender() -> Option<PathBuf> {
    let plat = env_dir("ProgramData", r"C:\ProgramData").join("Microsoft/Windows Defender/Platform");
    let mut found: Vec<PathBuf> = std::fs::read_dir(plat)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("MpCmdRun.exe"))
        .filter(|p| p.is_file())
        .collect();
    found.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    let exe = env_dir("ProgramFiles", r"C:\Program Files").join("Windows Defender/MpCmdRun.exe");
    found.pop().or_else(|| exe.exists().then_some(exe))
}

/// Exit code and stdout of `exe` scanning `path`, or why it could not run.
fn run(exe: &Path, path: &Path) -> Result<(i32, String), String> {
    let mut child = Command::new(exe)
        .args(["-Scan", "-ScanType", "3", "-File"])
        .arg(path)
        .arg("-DisableRemediation")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().ok_or("no output pipe")?;
    let reader = std::thread::spawn(move || {
        let mut buf = vec![];
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(s) => break s,
            None if start.elapsed() >= TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Defender timed out after {} seconds", TIMEOUT.as_secs()));
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    };
    let out = reader.join().unwrap_or_default();
    Ok((status.code().unwrap_or(-1), String::from_utf8_lossy(&out).into_owned()))
}

/// (Some(true), "") clean, (Some(false), detail) threat found, (None, detail) could not scan.
/// Defender looks inside zip/7z/rar.
pub fn scan(path: &Path) -> (Option<bool>, String) {
    judge(defender(), path, run)
}

fn judge(exe: Option<PathBuf>, path: &Path, run: impl Fn(&Path, &Path) -> Result<(i32, String), String>) -> (Option<bool>, String) {
    let Some(exe) = exe else {
        return (None, "Microsoft Defender is not available on this PC".into());
    };
    let (code, out) = match run(&exe, path) {
        Ok(r) => r,
        Err(e) => return (None, e),
    };
    if code == 0 {
        return (Some(true), String::new());
    }
    let lines: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if code == 2 {
        // Only the threat names: Defender's own lines hold the full path, with the Windows user name.
        let mut names: Vec<&str> = vec![];
        for l in &lines {
            if let (true, Some((_, name))) = (l.starts_with("Threat "), l.split_once(':')) {
                let name = name.trim();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        let detail = if names.is_empty() { "Threat found".to_string() } else { names.join(", ") };
        return (Some(false), detail);
    }
    match lines.last() {
        Some(last) => {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            (None, last.replace(&*path.to_string_lossy(), &name))
        }
        None => (None, format!("Defender exit code {code}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOUND: &str = "Scan starting...\nScan finished.\nScanning C:\\Users\\me\\job\\archive.zip found 2 threats.\n\
        ----------------------------- Threat information ------------------------------\n\
        Threat                  : Virus:DOS/EICAR_Test_File\nResources               : 1 total\n\
        Threat                  : Virus:DOS/EICAR_Test_File\nThreat                  : Trojan:Win32/Example\n";

    fn result(code: i32, out: &str) -> (Option<bool>, String) {
        let out = out.to_string();
        judge(Some("MpCmdRun.exe".into()), Path::new(r"C:\Users\me\job\archive.zip"), move |_, _| Ok((code, out.clone())))
    }

    // test_virus_scan.py Scan.test_results_name_the_threat_but_never_the_folder
    #[test]
    fn test_results_name_the_threat_but_never_the_folder() {
        assert_eq!(result(0, ""), (Some(true), String::new()));
        assert_eq!(result(2, FOUND), (Some(false), "Virus:DOS/EICAR_Test_File, Trojan:Win32/Example".into()));
        assert_eq!(result(2, "odd output"), (Some(false), "Threat found".into()));
        assert_eq!(result(1, r"Failed C:\Users\me\job\archive.zip"), (None, "Failed archive.zip".into()));
        assert_eq!(result(5, ""), (None, "Defender exit code 5".into()));
        assert_eq!(judge(None, Path::new("x"), |_, _| Ok((0, String::new()))).0, None);
    }
}
