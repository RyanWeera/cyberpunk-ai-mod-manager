//! The link pipe of the running window. Second app copies (started by nxm:// links) and the `--mcp` server
//! connect here. Both sides prove they know the token before a message is read.

use crate::controller::{Controller, lock};
use crate::pipe::{OwnerListener, Wire, answer_challenge, deliver_challenge};
use serde_json::Value;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How many connections are checked at once.
const CHECKS: usize = 4;

pub struct Serve {
    /// A secret: never print or log it.
    pub token: Vec<u8>,
    pub ctl: Arc<Controller>,
    pub events: Sender<(String, Value)>,
    /// What a taken nxm:// link starts (the controller's handle_nxm in the background).
    pub nxm: super::OnLink,
    /// Each connection gets this long for the handshake and its message.
    pub timeout: Mutex<Duration>,
    pub busy: AtomicUsize,
}

impl Serve {
    /// Take connections until the pipe closes; a stalled client blocks only its own thread.
    pub fn run(self: Arc<Self>, server: OwnerListener) {
        loop {
            match server.accept() {
                Ok(c) => {
                    let s = self.clone();
                    thread::spawn(move || s.take(Arc::new(c)));
                }
                Err(e) if e.kind() == io::ErrorKind::NotConnected => return, // the pipe was closed
                Err(_) => thread::sleep(Duration::from_millis(200)),         // one bad connection must not end the hand-over
            }
        }
    }

    /// Both sides prove the token; only then is the nxm link (or "show", or an AI-app tool call) read. At most
    /// four connections are checked at once and each gets `timeout`, so callers that stall cannot use up
    /// the app's threads.
    pub fn take(&self, conn: Arc<dyn Wire>) {
        if self.busy.fetch_add(1, SeqCst) >= CHECKS {
            self.busy.fetch_sub(1, SeqCst);
            conn.close();
            return;
        }
        let once = Arc::new(AtomicBool::new(false));
        let close = {
            let (c, o) = (conn.clone(), once.clone());
            // the timer and this thread both get here; a second close could hit a reused handle
            move || {
                if !o.swap(true, SeqCst) {
                    c.close();
                }
            }
        };
        let (cancel, cut) = mpsc::channel::<()>();
        let (t, timer_close) = (*lock(&self.timeout), close.clone());
        thread::spawn(move || {
            if cut.recv_timeout(t) == Err(RecvTimeoutError::Timeout) {
                timer_close();
            }
        });
        let mut cancel = Some(cancel);
        let r = (|| -> io::Result<Option<String>> {
            if self.ctl.stop.load(SeqCst) {
                return Ok(None); // closing: refuse, so the sender keeps its link and opens the next window
            }
            deliver_challenge(&*conn, &self.token)?;
            answer_challenge(&*conn, &self.token)?;
            let msg = String::from_utf8_lossy(&conn.recv(1 << 20)?).trim().to_string();
            if self.ctl.stop.load(SeqCst) {
                return Ok(None); // closing began meanwhile: no "ok", so the sender keeps its link
            }
            if msg.starts_with('{') {
                // a tool call from an AI app (ai_link::mcp); nxm hand-overs never start with "{"
                cancel.take(); // the caller proved the token; the tool may take longer than a handshake
                conn.send(ai_link::tools::run(&*self.ctl, &msg).to_string().as_bytes())?;
                return Ok(None);
            }
            conn.send(b"ok")?; // the sender counts the link as delivered only after this
            Ok(Some(msg))
        })();
        drop(cancel);
        close();
        self.busy.fetch_sub(1, SeqCst);
        let Ok(Some(msg)) = r else { return };
        let nxm = msg.to_lowercase().starts_with("nxm://");
        if nxm {
            (self.nxm)(&msg);
        }
        if nxm || msg == "show" {
            let _ = self.events.send(("show".into(), Value::Null));
        }
    }
}
