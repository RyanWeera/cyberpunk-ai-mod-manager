//! real hidden-browser click (Brave, Edge or Chrome must be installed) on local pages that copy Nexus.
//! Run `cargo test -p downloads --test real_browser -- --ignored`.
use downloads::nexus_browser::{Browser, CLICK_JS, Driver, HOOK_JS, LOGIN_JS, find_browser};
use std::io::{Read, Write};
use std::net::TcpListener;

/// Visible windows of process `pid` that the taskbar shows (not tool windows).
fn taskbar_windows(pid: u32) -> usize {
    let out = std::process::Command::new(shared::sys32().join("WindowsPowerShell/v1.0/powershell.exe"))
        .args(["-NoProfile", "-NonInteractive", "-Command", TASKBAR_PS])
        .env("CP_PID", pid.to_string())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}
const TASKBAR_PS: &str = concat!(
    "Add-Type -Name W -Namespace U -MemberDefinition '",
    "[DllImport(\"user32.dll\")] public static extern bool EnumWindows(EnumProc f, IntPtr l);",
    "public delegate bool EnumProc(IntPtr h, IntPtr l);",
    "[DllImport(\"user32.dll\")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint p);",
    "[DllImport(\"user32.dll\")] public static extern bool IsWindowVisible(IntPtr h);",
    "[DllImport(\"user32.dll\")] public static extern IntPtr GetWindowLongPtrW(IntPtr h, int i);';",
    "$n=0;$want=[uint32]$env:CP_PID;[U.W]::EnumWindows({param($h,$l)$p=0;[void][U.W]::GetWindowThreadProcessId($h,[ref]$p);",
    "if($p -eq $want -and [U.W]::IsWindowVisible($h) -and ([int64][U.W]::GetWindowLongPtrW($h,-20) -band 0x80) -eq 0){$script:n++};$true},[IntPtr]::Zero)|Out-Null;$n"
);

const LINK: &str = "nxm://cyberpunk2077/mods/1/files/2?key=abc&expires=1&user_id=1";

/// Serve `page` at / (and a 302 to LINK at /go), click Slow download in the hidden browser, return the link.
fn click_on(page: &'static str) -> Option<String> {
    assert!(find_browser().is_some(), "no Brave, Edge or Chrome on this PC");
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", server.local_addr().unwrap());
    std::thread::spawn(move || {
        for mut s in server.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let answer = if req.starts_with("GET /go") {
                format!("HTTP/1.1 302 Found\r\nLocation: {LINK}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            } else {
                format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len())
            };
            let _ = s.write_all(answer.as_bytes());
        }
    });
    let profile = testkit::TempDir::new();
    let mut b = Browser::new(profile.path(), false);
    let got = (|| -> Result<Option<String>, String> {
        b.start("about:blank").map_err(|e| format!("start: {e}"))?;
        let mut t = b.open_tab(&url).map_err(|e| format!("open: {e}"))?;
        t.wait_ready_for(30).map_err(|e| format!("ready: {e}"))?;
        let pid = b.pid().expect("browser runs");
        assert!(taskbar_windows(pid) == 0, "the hidden browser shows a window on the taskbar");
        assert_ne!(t.run_js(LOGIN_JS)?, 1, "a page with no sign-in link was taken as logged out");
        assert_eq!(t.run_js(HOOK_JS)?, 1);
        assert_eq!(t.run_js(CLICK_JS)?, 1, "the Slow download button was not clicked");
        t.wait_nxm_for(20)
    })();
    b.quit();
    got.unwrap()
}

#[test]
#[ignore]
fn hidden_click_on_a_link_returns_the_nxm_link() {
    assert_eq!(click_on("<html><body><h1>Files</h1><a href='/go'>Slow download</a></body></html>").as_deref(), Some(LINK));
}

/// The Nexus page since 2026-11: the button is in the open shadow root of `mod-download-modal`, it appears after
/// the page load, and its click waits for a countdown, then sets location.href to the nxm:// link.
#[test]
#[ignore]
fn hidden_click_in_a_shadow_root_returns_the_nxm_link() {
    const PAGE: &str = concat!(
        "<html><body><h1>Files</h1><mod-download-modal></mod-download-modal><script>",
        "setTimeout(function(){var r=document.querySelector('mod-download-modal').attachShadow({mode:'open'});",
        "r.innerHTML='<button type=\"button\"><span>Slow download</span></button>';",
        "r.querySelector('button').addEventListener('click',function(){",
        "setTimeout(function(){window.location.href='nxm://cyberpunk2077/mods/1/files/2?key=abc&expires=1&user_id=1';},2000);});",
        "},1000);</script></body></html>"
    );
    assert_eq!(click_on(PAGE).as_deref(), Some(LINK));
}
