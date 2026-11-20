//! The hidden-browser auto-clicker: WebSocket framing, the nxm:// capture and the Clicker flow
//! (Python `tests/test_browser.py`; the controller wiring tests come with the controller).
use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// test_browser.py WsFraming.test_round_trip
#[test]
fn test_round_trip() {
    for size in [0, 5, 125, 126, 1000, 65535, 65536, 200001] {
        let buf = ws_frame(&vec![b'x'; size], 1);
        let (got, rest) = ws_message(&buf);
        let (op, fin, data) = got.expect("a whole frame");
        assert_eq!((op, fin), (1, true));
        assert_eq!(data, vec![b'x'; size]);
        assert!(rest.is_empty());
    }
}

// test_browser.py WsFraming.test_partial_buffer_waits
#[test]
fn test_partial_buffer_waits() {
    let buf = ws_frame(b"hello", 1);
    let (got, _) = ws_message(&buf[..3]);
    assert!(got.is_none());
    let joined = [&buf[..3], &buf[3..]].concat();
    let (got, rest) = ws_message(&joined);
    assert_eq!(got.unwrap().2, b"hello");
    assert!(rest.is_empty());
}

// test_browser.py WsFraming.test_ping_opcode
#[test]
fn test_ping_opcode() {
    let (got, _) = ws_message(&ws_frame(b"pin", 0x9));
    assert_eq!(got.unwrap().0, 0x9);
}

// test_browser.py WsFraming.test_unmasked_server_frames
#[test]
fn test_unmasked_server_frames() {
    let (got, rest) = ws_message(b"\x81\x05hello");
    assert_eq!(got, Some((1, true, b"hello".to_vec())));
    assert!(rest.is_empty());
    let (got, _) = ws_message(b"\x01\x03hel");
    assert_eq!(got, Some((1, false, b"hel".to_vec())));
    let long = [&b"\x81\x7e\x00\x80"[..], &[b'y'; 128]].concat();
    assert_eq!(ws_message(&long).0, Some((1, true, vec![b'y'; 128])));
}

// test_browser.py WsFraming.test_recv_reads_the_socket
#[test]
fn test_recv_reads_the_socket() {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (mut b, _) = l.accept().unwrap();
    let mut ws = WS { sock: a, buf: vec![], parts: vec![] };
    assert_eq!(ws.recv(Duration::from_millis(200)).unwrap(), None);
    b.write_all(b"\x01\x03{\"a").unwrap(); // a fragmented message, sent in two parts
    let mut b2 = b.try_clone().unwrap();
    let later = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        b2.write_all(b"\x80\x04\":1}").unwrap();
    });
    assert_eq!(ws.recv(Duration::from_secs(2)).unwrap().as_deref(), Some("{\"a\":1}"));
    later.join().unwrap();
    b.write_all(b"\x89\x00").unwrap(); // a ping is answered with a pong, then reading goes on
    assert_eq!(ws.recv(Duration::from_millis(200)).unwrap(), None);
    b.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut pong = [0u8; 64];
    let n = b.read(&mut pong).unwrap();
    assert_eq!(ws_message(&pong[..n]).0.unwrap().0, 0xA);
    drop(b);
    assert!(ws.recv(Duration::from_secs(1)).is_err());
}

#[test]
fn test_default_browser_prog_ids() {
    assert_eq!(browser_of("BraveHTML"), Some("brave"));
    assert_eq!(browser_of("MSEdgeHTM"), Some("msedge"));
    assert_eq!(browser_of("ChromeHTML.ABC123"), Some("chrome"));
    assert_eq!(browser_of("FirefoxURL-308046B0AF4A39CB"), None);
    assert!(BROWSERS.iter().any(|(_, t)| t.ends_with(r"\brave.exe")), "find_browser matches a default browser by its file name");
}

// test_browser.py ScanNxm.test_finds_it_anywhere
#[test]
fn test_finds_it_anywhere() {
    let deep = json!({"a": [{"b": null}, "nxm://cyberpunk2077/mods/1/files/2?key=k&expires=9"]});
    assert_eq!(scan_nxm(&json!({"params": deep})), "nxm://cyberpunk2077/mods/1/files/2?key=k&expires=9");
    assert_eq!(scan_nxm(&json!({"params": {"x": "https://cdn.example/p.zip"}})), "");
}

/// What a fake page answers. LOGIN_JS answers come from `login` in turn (the last one repeats).
struct Script {
    login: Vec<i64>,
    click: i64,
    nxm: Option<String>,
}

/// A DevTools tab that answers from a shared script.
struct FakeTab {
    script: Arc<Mutex<Script>>,
    closed: Arc<AtomicBool>,
}

impl Page for FakeTab {
    fn wait_ready(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn run(&mut self, js: &str) -> Result<Value, String> {
        let mut s = self.script.lock().unwrap();
        Ok(json!(match js {
            LOGIN_JS if s.login.len() > 1 => s.login.remove(0),
            LOGIN_JS => s.login[0],
            CLICK_JS => s.click,
            _ => 0,
        }))
    }

    fn wait_nxm(&mut self) -> Result<Option<String>, String> {
        Ok(self.script.lock().unwrap().nxm.clone())
    }

    fn close(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

struct FakeBrowser {
    script: Arc<Mutex<Script>>,
    closed: Arc<AtomicBool>,
    opens: Arc<AtomicUsize>,
    alive: bool,
    login_ok: bool,
}

impl Driver for FakeBrowser {
    fn alive(&mut self) -> bool {
        self.alive
    }

    fn open(&mut self, _url: &str) -> Result<Box<dyn Page>, String> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeTab { script: self.script.clone(), closed: self.closed.clone() }))
    }

    fn wait_login(&mut self, _seconds: u64) -> bool {
        self.login_ok
    }

    fn quit(&mut self) {
        self.alive = false;
    }
}

type Seen = Arc<Mutex<Vec<String>>>;

#[derive(Default)]
struct Run {
    logs: Seen,
    links: Seen,
    fails: Seen,
    closed: Arc<AtomicBool>,
    opens: Arc<AtomicUsize>,
}

/// A Clicker whose browsers (hidden, and the login window that answers `login_ok`) are fakes over one script.
fn clicker(script: Script, login_ok: bool) -> (Clicker, Run) {
    let script = Arc::new(Mutex::new(script));
    let run = Run::default();
    let mut c = Clicker::new(Path::new("p"));
    let (logs, links, fails) = (run.logs.clone(), run.links.clone(), run.fails.clone());
    c.log = Box::new(move |m| logs.lock().unwrap().push(m.into()));
    c.on_link = Box::new(move |l| {
        links.lock().unwrap().push(l.into());
        Ok(())
    });
    c.on_fail = Box::new(move |_, w| fails.lock().unwrap().push(w.into()));
    let (closed, opens) = (run.closed.clone(), run.opens.clone());
    c.launch = Box::new(move |_, _, _| {
        let b = FakeBrowser { script: script.clone(), closed: closed.clone(), opens: opens.clone(), alive: true, login_ok };
        Ok(Box::new(b) as Box<dyn Driver>)
    });
    (c, run)
}

fn job() -> Value {
    json!({"src": {"mod_id": 1, "file_id": 2}, "state": "waiting"})
}

fn script(login: &[i64], nxm: Option<&str>) -> Script {
    Script { login: login.to_vec(), click: 1, nxm: nxm.map(String::from) }
}

fn got(v: &Seen) -> Vec<String> {
    v.lock().unwrap().clone()
}

// test_browser.py ClickerFlow.test_success_hands_the_nxm_link_over
#[test]
fn test_success_hands_the_nxm_link_over() {
    let (c, r) = clicker(script(&[0], Some("nxm://cyberpunk2077/mods/1/files/2?key=k&expires=9")), true);
    c.click(&job());
    assert_eq!(got(&r.links), ["nxm://cyberpunk2077/mods/1/files/2?key=k&expires=9"]);
    assert!(got(&r.fails).is_empty() && got(&r.logs).is_empty());
    assert!(r.closed.load(Ordering::SeqCst));
}

#[test]
fn test_an_unknown_login_state_still_clicks() {
    let (c, r) = clicker(script(&[2], Some("nxm://cyberpunk2077/mods/1/files/2")), true);
    c.click(&job());
    assert_eq!(got(&r.links), ["nxm://cyberpunk2077/mods/1/files/2"]);
    assert_eq!(r.opens.load(Ordering::SeqCst), 1); // no login window
}

// test_browser.py ClickerFlow.test_a_skipped_job_loses_its_link
#[test]
fn test_a_skipped_job_loses_its_link() {
    let (mut c, r) = clicker(script(&[0], Some("nxm://cyberpunk2077/mods/1/files/2")), true);
    c.waiting = Box::new(|_| false);
    c.click(&job());
    assert!(got(&r.links).is_empty() && got(&r.fails).is_empty());
}

// test_browser.py ClickerFlow.test_clicks_run_one_at_a_time
#[test]
fn test_clicks_run_one_at_a_time() {
    let (c, r) = clicker(script(&[0], Some("nxm://cyberpunk2077/mods/1/files/2")), true);
    let c = Arc::new(c);
    let held = lock(&c.busy); // a click is running
    let c2 = c.clone();
    let t = std::thread::spawn(move || c2.click(&job()));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!t.is_finished()); // the second click waits
    assert_eq!(r.opens.load(Ordering::SeqCst), 0);
    drop(held);
    t.join().unwrap();
    assert_eq!(r.opens.load(Ordering::SeqCst), 1);
}

// test_browser.py ClickerFlow.test_clicks_keep_a_gap_and_closing_ends_the_wait
#[test]
fn test_clicks_keep_a_gap_and_closing_ends_the_wait() {
    let (mut c, r) = clicker(script(&[0], Some("nxm://cyberpunk2077/mods/1/files/2")), true);
    c.gap = Duration::from_millis(400);
    let start = Instant::now();
    c.click(&job());
    c.click(&job());
    assert!(start.elapsed() >= Duration::from_millis(400)); // the second click waited
    assert_eq!(r.opens.load(Ordering::SeqCst), 2);
    // Python sets c.gap = 60 on the same clicker; the gap is a plain field here, so a second clicker has 60 seconds
    let (mut c, r) = clicker(script(&[0], Some("nxm://cyberpunk2077/mods/1/files/2")), true);
    c.gap = Duration::from_secs(60);
    c.click(&job());
    let c = Arc::new(c);
    let c2 = c.clone();
    let t = std::thread::spawn(move || c2.click(&job()));
    std::thread::sleep(Duration::from_millis(100));
    c.close(); // the window closes during the wait
    let end = Instant::now() + Duration::from_secs(2);
    while !t.is_finished() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(t.is_finished());
    assert_eq!(r.opens.load(Ordering::SeqCst), 1); // no click after close
}

// test_browser.py ClickerFlow.test_a_link_for_another_file_falls_back
#[test]
fn test_a_link_for_another_file_falls_back() {
    let (c, r) = clicker(script(&[0], Some("nxm://cyberpunk2077/mods/1/files/23?key=k")), true);
    c.click(&job());
    assert!(got(&r.links).is_empty());
    assert_eq!(got(&r.fails), ["Nexus gave a download link for another file"]);
}

// test_browser.py ClickerFlow.test_an_error_while_handing_the_link_over_falls_back
#[test]
fn test_an_error_while_handing_the_link_over_falls_back() {
    let (mut c, r) = clicker(script(&[0], Some("nxm://cyberpunk2077/mods/1/files/2")), true);
    c.on_link = Box::new(|_| Err("bad link".into()));
    c.click(&job());
    assert_eq!(got(&r.fails), ["bad link"]);
}

// test_browser.py ClickerFlow.test_no_link_after_the_click_falls_back
#[test]
fn test_no_link_after_the_click_falls_back() {
    let (c, r) = clicker(script(&[0], None), true);
    c.click(&job());
    assert_eq!(got(&r.fails), ["Nexus gave no download link after the click"]);
    assert!(got(&r.links).is_empty());
}

// test_browser.py ClickerFlow.test_login_needed_and_refused_falls_back
#[test]
fn test_login_needed_and_refused_falls_back() {
    let (c, r) = clicker(script(&[1], Some("nxm://cyberpunk2077/mods/1/files/2")), false);
    c.click(&job());
    let fails = got(&r.fails);
    assert_eq!(fails.len(), 1);
    assert!(fails[0].contains("login"));
    assert!(got(&r.links).is_empty());
}

// test_browser.py ClickerFlow.test_after_one_login_the_click_goes_on
#[test]
fn test_after_one_login_the_click_goes_on() {
    // logged out on the first try, logged in on the retry
    let (c, r) = clicker(script(&[1, 0], Some("nxm://cyberpunk2077/mods/1/files/2")), true);
    c.click(&job());
    assert_eq!(got(&r.links).len(), 1); // the second try, after the login, hands the link over
    assert!(got(&r.fails).is_empty());
}

// test_browser.py FindBrowser.test_brave_first_with_its_own_profile
#[test]
fn test_brave_first_with_its_own_profile() {
    let dir = testkit::TempDir::new();
    let tmp = dir.path();
    for tail in [BRAVE, r"Microsoft\Edge\Application\msedge.exe"] {
        std::fs::create_dir_all(tmp.join(tail).parent().unwrap()).unwrap();
        std::fs::write(tmp.join(tail), b"").unwrap();
    }
    let vars = ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"];
    let old: Vec<_> = vars.iter().map(std::env::var_os).collect();
    // SAFETY: tests run on one thread (RUST_TEST_THREADS=1), so nothing reads the environment meanwhile
    unsafe {
        std::env::set_var("ProgramFiles", tmp);
        std::env::set_var("ProgramFiles(x86)", "");
        std::env::set_var("LocalAppData", "");
    }
    // with no default browser among the three (the PC's own default would decide otherwise)
    let brave = find_browser_from(None);
    let default_edge = find_browser_from(Some("msedge"));
    std::fs::remove_file(tmp.join(BRAVE)).unwrap();
    let edge = find_browser_from(Some("brave"));
    for (k, v) in vars.iter().zip(old) {
        // SAFETY: as above
        unsafe {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
    assert_eq!(brave, Some(tmp.join(BRAVE)));
    assert!(default_edge.unwrap().to_string_lossy().ends_with("msedge.exe")); // the default browser comes first
    let edge = edge.unwrap(); // a default browser that is not installed falls back to the order
    assert!(edge.to_string_lossy().ends_with("msedge.exe"));
    assert_eq!(profile_for(brave.as_deref(), &tmp.join("p")), tmp.join("p").join("brave"));
    assert_eq!(profile_for(Some(&edge), &tmp.join("p")), tmp.join("p")); // Edge keeps the old folder
}

// test_browser.py CloseTab.test_plain_text_close_answer_is_not_an_error
#[test]
fn test_plain_text_close_answer_is_not_an_error() {
    use std::io::{Read, Write};
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = srv.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Some(mut s) = srv.incoming().flatten().next() {
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 17\r\nConnection: close\r\n\r\nTarget is closing");
        }
    });
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (mut b, _) = l.accept().unwrap();
    let ws = WS { sock: a, buf: vec![], parts: vec![] };
    let mut tab = Tab { target: serde_json::json!({"id": "T1"}), events: vec![], ws, port, next_id: 0 };
    Page::close(&mut tab); // must not fail on the plain-text answer
    b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut got = [0u8; 16];
    let n = b.read(&mut got).unwrap();
    assert_eq!(ws_message(&got[..n]).0.unwrap().0, 0x8, "the WebSocket gets a close frame");
}

/// A stand-in for a leftover browser: a sleeping cmd.exe whose command line names the profile folder.
fn fake_browser(profile: &Path) -> std::process::Child {
    Command::new(shared::sys32().join("cmd.exe"))
        .arg("/c")
        .arg(format!("ping -n 60 127.0.0.1 >nul & rem --user-data-dir={}", profile.display()))
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn test_a_leftover_browser_on_the_profile_is_ended() {
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("browser");
    let mut leftover = fake_browser(&profile);
    // another browser's folder inside this one stays open
    let mut other = fake_browser(&profile.join("brave"));
    kill_leftovers(&profile);
    let deadline = Instant::now() + Duration::from_secs(5);
    while matches!(leftover.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let ended = leftover.try_wait().unwrap().is_some();
    let kept = other.try_wait().unwrap().is_none();
    kill_leftovers(&profile.join("brave")); // ends cmd.exe and its ping before an assert can fail
    assert!(ended);
    assert!(kept);
}
