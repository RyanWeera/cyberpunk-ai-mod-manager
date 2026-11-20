//! Port of tests/test_app.py: the start-up hand-over on a private test pipe; the window itself is a stub.

use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use testkit::TempDir;
use windows_sys::Win32::Security::Authorization::{ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

const LINK: &str = "nxm://cyberpunk2077/mods/1/files/2?key=a&expires=1";

struct Fx {
    _tmp: TempDir,
    pipe: String,
    token: PathBuf,
    args: Vec<String>,
}

fn fx() -> Fx {
    static N: AtomicUsize = AtomicUsize::new(0);
    let tmp = TempDir::new();
    let token = tmp.path().join("ipc.token");
    fs::write(&token, "secret").unwrap();
    let pipe = format!(r"\\.\pipe\cp2077mm-test-{}-{}-{}", std::process::id(), N.fetch_add(1, SeqCst), token_hex(4));
    Fx { _tmp: tmp, pipe, token, args: vec![LINK.to_string()] }
}

type Ran = Arc<Mutex<Vec<Option<String>>>>;

impl Fx {
    /// A running window: proves the token, takes links, confirms "ok" (like ui's take).
    fn window(&self, got: Arc<Mutex<Vec<String>>>, delay: Duration) {
        let ls = OwnerListener::new(&self.pipe).unwrap();
        thread::spawn(move || {
            thread::sleep(delay);
            while let Ok(c) = ls.accept() {
                if deliver_challenge(&c, b"secret").and_then(|()| answer_challenge(&c, b"secret")).is_err() {
                    continue;
                }
                if let Ok(m) = c.recv(1 << 20) {
                    got.lock().unwrap().push(String::from_utf8_lossy(&m).into_owned());
                    let _ = c.send(b"ok");
                }
            }
        });
    }

    fn main(&self, wait: Duration, boxes: &AtomicUsize) -> Ran {
        let ran: Ran = Arc::default();
        let r = ran.clone();
        start(Start {
            pipe: &self.pipe,
            token: &self.token,
            args: &self.args,
            wait,
            run_window: Box::new(move |server, url, _token| {
                server.close();
                r.lock().unwrap().push(url);
            }),
            message_box: &|| {
                boxes.fetch_add(1, SeqCst);
            },
        });
        ran
    }
}

fn no_spawn() {
    panic!("a window was started");
}

// test_ai_tool_call_goes_to_the_window
#[test]
fn ai_tool_call_goes_to_the_window() {
    let f = fx();
    let ls = OwnerListener::new(&f.pipe).unwrap();
    let got = Arc::new(Mutex::new(vec![]));
    let g = got.clone();
    thread::spawn(move || {
        for reply in [json!({"text": "3 mods"}), json!({"error": "busy"})] {
            let c = ls.accept().unwrap();
            deliver_challenge(&c, b"secret").unwrap();
            answer_challenge(&c, b"secret").unwrap();
            g.lock().unwrap().push(serde_json::from_slice::<Value>(&c.recv(1 << 20).unwrap()).unwrap());
            c.send(reply.to_string().as_bytes()).unwrap();
        }
    });
    let ask = |tool| ask_window_with(&f.pipe, &f.token, tool, &json!({}), Duration::from_secs(40), &no_spawn);
    assert_eq!(ask("status"), Ok("3 mods".to_string()));
    assert_eq!(ask("sort_load_order"), Err("busy".to_string()));
    assert_eq!(*got.lock().unwrap(), vec![json!({"tool": "status", "args": {}}), json!({"tool": "sort_load_order", "args": {}})]);
}

// test_ai_tool_call_starts_the_window_once
#[test]
fn ai_tool_call_starts_the_window_once() {
    let f = fx();
    let n = AtomicUsize::new(0);
    let r = ask_window_with(&f.pipe, &f.token, "status", &json!({}), Duration::from_millis(1200), &|| {
        n.fetch_add(1, SeqCst);
    });
    assert!(r.unwrap_err().contains("did not start"));
    assert_eq!(n.load(SeqCst), 1); // tried again until the deadline, but started only one copy
}

// test_ai_tool_call_starts_no_copy_while_a_window_runs
#[test]
fn ai_tool_call_starts_no_copy_while_a_window_runs() {
    use windows_sys::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_MESSAGE};
    // all pipe instances busy (231): a pipe with one instance that a client already holds
    let f = fx();
    let w = wide(&f.pipe);
    // SAFETY: a plain one-instance pipe for this test.
    let h = unsafe { CreateNamedPipeW(w.as_ptr(), windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX, PIPE_TYPE_MESSAGE, 1, 512, 512, 0, std::ptr::null()) };
    assert!(h != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE);
    let _holder = Conn::connect(&f.pipe).unwrap();
    let r = ask_window_with(&f.pipe, &f.token, "status", &json!({}), Duration::from_millis(600), &no_spawn);
    assert!(r.unwrap_err().contains("did not answer"));
    // a refused token ("digest was wrong")
    let g = fx();
    let ls = OwnerListener::new(&g.pipe).unwrap();
    thread::spawn(move || {
        while let Ok(c) = ls.accept() {
            let _ = deliver_challenge(&c, b"guess");
        }
    });
    let r = ask_window_with(&g.pipe, &g.token, "status", &json!({}), Duration::from_millis(600), &no_spawn);
    let e = r.unwrap_err();
    assert!(e.contains("did not answer") && e.contains("did not start"), "{e}");
}

// test_window_pipe_is_open_to_this_account_only
#[test]
fn window_pipe_is_open_to_this_account_only() {
    let f = fx();
    let server = Arc::new(OwnerListener::new(&f.pipe).unwrap());
    // SAFETY: the out pointers get a descriptor and a string that this test reads and leaves to the process.
    let sddl = unsafe {
        let mut sd = std::ptr::null_mut();
        let rc = GetSecurityInfo(
            server.first_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        );
        assert_eq!(rc, 0);
        let mut s = std::ptr::null_mut();
        assert!(ConvertSecurityDescriptorToStringSecurityDescriptorW(sd, 1, DACL_SECURITY_INFORMATION, &mut s, std::ptr::null_mut()) != 0);
        let mut n = 0;
        while *s.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(s, n))
    };
    assert_eq!(sddl, "D:P(A;;FA;;;OW)"); // the owner only: no Everyone (WD) or Anonymous (AN) read
    let s = server.clone();
    let t = thread::spawn(move || s.accept().unwrap().recv(1 << 20).unwrap());
    let c = Conn::connect(&f.pipe).unwrap(); // this account still gets in
    c.send(b"show").unwrap();
    assert_eq!(t.join().unwrap(), b"show");
}

// test_running_window_takes_the_link
#[test]
fn running_window_takes_the_link() {
    let f = fx();
    let got = Arc::new(Mutex::new(vec![]));
    f.window(got.clone(), Duration::ZERO);
    let ran = f.main(Duration::from_secs(15), &AtomicUsize::new(0));
    assert!(ran.lock().unwrap().is_empty()); // no second window
    assert_eq!(*got.lock().unwrap(), vec![LINK.to_string()]);
}

// test_slow_window_gets_the_link_once
#[test]
fn slow_window_gets_the_link_once() {
    let f = fx();
    let got = Arc::new(Mutex::new(vec![]));
    f.window(got.clone(), Duration::from_millis(6500)); // takes the pipe, then builds its UI for 6.5 s before serving
    let ran = f.main(Duration::from_secs(15), &AtomicUsize::new(0));
    thread::sleep(Duration::from_millis(1500)); // a second attempt would arrive now
    assert!(ran.lock().unwrap().is_empty());
    assert_eq!(*got.lock().unwrap(), vec![LINK.to_string()]);
}

// test_pipe_holder_that_never_answers_cannot_hang_start_up
#[test]
fn pipe_holder_that_never_answers_cannot_hang_start_up() {
    let f = fx();
    let silent = OwnerListener::new(&f.pipe).unwrap();
    thread::spawn(move || {
        let _c = silent.accept(); // takes the call, then says nothing
        thread::sleep(Duration::from_secs(60));
    });
    let t = Instant::now();
    let boxes = AtomicUsize::new(0);
    let ran = f.main(Duration::from_secs(2), &boxes);
    assert!(t.elapsed() < Duration::from_secs(5));
    assert_eq!(boxes.load(SeqCst), 1); // "still running and did not answer"
    assert!(ran.lock().unwrap().is_empty());
}

// test_link_refused_by_a_closing_window_opens_the_next_window
#[test]
fn link_refused_by_a_closing_window_opens_the_next_window() {
    let f = fx();
    let closing = Arc::new(OwnerListener::new(&f.pipe).unwrap());
    let tries = Arc::new(AtomicUsize::new(0));
    let (c, n) = (closing.clone(), tries.clone());
    thread::spawn(move || {
        // like the window's take while it closes: accept, then hang up at once
        while let Ok(conn) = c.accept() {
            drop(conn);
            n.fetch_add(1, SeqCst);
        }
    });
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(1)); // the closing window is gone after 1 s
        closing.close();
    });
    let ran = f.main(Duration::from_secs(15), &AtomicUsize::new(0));
    assert!(tries.load(SeqCst) <= 4, "{}", tries.load(SeqCst)); // paced retries (about 2 a second), not a busy loop
    assert_eq!(*ran.lock().unwrap(), vec![Some(LINK.to_string())]); // one window, and the link was kept
    assert_ne!(fs::read_to_string(&f.token).unwrap(), "secret"); // this window wrote its own token after taking the pipe
}
