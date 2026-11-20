//! The per-user link pipe: an owner-only, message-mode named pipe and the token handshake of Python's
//! `multiprocessing.connection` (HMAC-SHA256 challenge, `{sha256}` prefix), so a Rust and a Python copy of
//! the app can talk to each other.

use sha2::{Digest, Sha256};
use shared::win::wide;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::io;
use std::ptr::{null, null_mut};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, NMPWAIT_WAIT_FOREVER, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, SetNamedPipeHandleState, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::CreateEventW;

/// Python's `multiprocessing.connection.BUFSIZE`.
const BUFSIZE: u32 = 8192;
const CHALLENGE: &[u8] = b"#CHALLENGE#";
const WELCOME: &[u8] = b"#WELCOME#";
const FAILURE: &[u8] = b"#FAILURE#";
/// Only the account that owns the pipe (the one that started the window) may open it.
const OWNER_ONLY: &str = "D:P(A;;GA;;;OW)";

/// One message-mode connection. `close` may run on another thread (the cut-off timer): it stops a read or
/// write in progress, and the handle itself is closed once, on drop.
pub trait Wire: Send + Sync {
    fn send(&self, msg: &[u8]) -> io::Result<()>;
    /// One whole message; longer than `max` bytes is an error (Python: "bad message length").
    fn recv(&self, max: usize) -> io::Result<Vec<u8>>;
    fn close(&self);
}

/// Random bytes from the system generator.
pub fn random(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    // SAFETY: the buffer holds n bytes; a null algorithm handle with this flag uses the system RNG.
    let st = unsafe { BCryptGenRandom(null_mut(), b.as_mut_ptr(), n as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    assert_eq!(st, 0, "the system random number generator failed");
    b
}

/// `secrets.token_hex(n)`.
pub fn token_hex(n: usize) -> String {
    random(n).iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |x: u8| k.iter().map(|b| b ^ x).collect::<Vec<u8>>();
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(msg).finalize();
    Sha256::new().chain_update(pad(0x5c)).chain_update(inner).finalize().to_vec()
}

fn auth_err(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, m.to_string())
}

/// Python's `deliver_challenge`: prove that the other side knows `key`.
pub fn deliver_challenge(c: &dyn Wire, key: &[u8]) -> io::Result<()> {
    let mut message = b"{sha256}".to_vec();
    message.extend(random(40));
    c.send(&[CHALLENGE, &message].concat())?;
    let response = c.recv(256)?;
    let ok = response.strip_prefix(b"{sha256}").is_some_and(|mac| {
        let want = hmac_sha256(key, &message);
        mac.len() == want.len() && mac.iter().zip(&want).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
    });
    if !ok {
        c.send(FAILURE)?;
        return Err(auth_err("digest received was wrong"));
    }
    c.send(WELCOME)
}

/// Python's `answer_challenge`: prove to the other side that this side knows `key`.
pub fn answer_challenge(c: &dyn Wire, key: &[u8]) -> io::Result<()> {
    let message = c.recv(256)?;
    let Some(message) = message.strip_prefix(CHALLENGE) else { return Err(auth_err("Protocol error, expected challenge")) };
    if !message.starts_with(b"{sha256}") {
        return Err(auth_err("unsupported digest"));
    }
    c.send(&[b"{sha256}".as_slice(), &hmac_sha256(key, message)].concat())?;
    if c.recv(256)? != WELCOME {
        return Err(auth_err("digest sent was rejected"));
    }
    Ok(())
}

struct Handle(HANDLE);
// SAFETY: a kernel handle may be used from any thread.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

/// A pipe connection (either end) opened for overlapped I/O.
pub struct Conn {
    h: Handle,
    ev: Handle,
    server: bool,
    closed: AtomicBool,
}

fn event() -> io::Result<Handle> {
    // SAFETY: plain manual-reset event without a name.
    let ev = unsafe { CreateEventW(null(), 1, 0, null()) };
    if ev.is_null() { Err(io::Error::last_os_error()) } else { Ok(Handle(ev)) }
}

impl Conn {
    fn new(h: HANDLE, server: bool) -> io::Result<Conn> {
        let ev = event().inspect_err(|_| {
            // SAFETY: h is an open handle that nobody else holds.
            unsafe { CloseHandle(h) };
        })?;
        Ok(Conn { h: Handle(h), ev, server, closed: AtomicBool::new(false) })
    }

    /// Open the pipe as a client, in message read mode. A missing pipe gives `NotFound`, a busy one an error.
    pub fn connect(name: &str) -> io::Result<Conn> {
        let w = wide(name);
        // SAFETY: w is a NUL-ended wide string.
        unsafe {
            if WaitNamedPipeW(w.as_ptr(), 1000) == 0 && GetLastError() == ERROR_FILE_NOT_FOUND {
                return Err(io::Error::from_raw_os_error(ERROR_FILE_NOT_FOUND as i32));
            }
            // identification level only: a pipe server squatting the name cannot act as this user
            use windows_sys::Win32::Storage::FileSystem::{SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT};
            let fl = FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION;
            let h = CreateFileW(w.as_ptr(), GENERIC_READ | GENERIC_WRITE, 0, null(), OPEN_EXISTING, fl, null_mut());
            if h == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let c = Conn::new(h, false)?;
            let mode = PIPE_READMODE_MESSAGE;
            if SetNamedPipeHandleState(h, &mode, null(), null()) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(c)
        }
    }

    /// One overlapped read or write; Ok((bytes, more)) where `more` means the message has more bytes.
    fn io(&self, read: Option<&mut [u8]>, data: &[u8]) -> io::Result<(u32, bool)> {
        if self.closed.load(SeqCst) {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        // SAFETY: ov and the buffers live until GetOverlappedResult (wait = TRUE) returns.
        unsafe {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.ev.0;
            let mut n = 0u32;
            let ok = match read {
                Some(b) => ReadFile(self.h.0, b.as_mut_ptr(), b.len() as u32, null_mut(), &mut ov),
                None => WriteFile(self.h.0, data.as_ptr(), data.len() as u32, null_mut(), &mut ov),
            };
            if ok == 0 {
                let e = GetLastError();
                if e != ERROR_IO_PENDING && e != ERROR_MORE_DATA {
                    return Err(io::Error::from_raw_os_error(e as i32));
                }
            }
            if GetOverlappedResult(self.h.0, &ov, &mut n, 1) == 0 {
                let e = GetLastError();
                if e == ERROR_MORE_DATA {
                    return Ok((n, true));
                }
                return Err(io::Error::from_raw_os_error(e as i32));
            }
            Ok((n, false))
        }
    }
}

impl Wire for Conn {
    fn send(&self, msg: &[u8]) -> io::Result<()> {
        let (n, _) = self.io(None, msg)?;
        if n as usize != msg.len() {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        Ok(())
    }

    fn recv(&self, max: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![];
        let mut buf = vec![0u8; (max + 1).min(BUFSIZE as usize)];
        loop {
            let (n, more) = self.io(Some(&mut buf), &[])?;
            out.extend_from_slice(&buf[..n as usize]);
            if out.len() > max {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "bad message length"));
            }
            if !more {
                if out.is_empty() {
                    return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
                }
                return Ok(out);
            }
        }
    }

    fn close(&self) {
        if self.closed.swap(true, SeqCst) {
            return;
        }
        // SAFETY: the handle stays open until drop; this only stops its I/O and ends the client's link.
        unsafe {
            CancelIoEx(self.h.0, null());
            if self.server {
                DisconnectNamedPipe(self.h.0);
            }
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.close();
        // SAFETY: both handles belong to this Conn only.
        unsafe {
            CloseHandle(self.h.0);
            CloseHandle(self.ev.0);
        }
    }
}

/// Python's `PipeListener` with the owner-only DACL: one waiting pipe instance at a time, the next made
/// before a client takes the current one.
pub struct OwnerListener {
    name: String,
    queue: Mutex<VecDeque<Handle>>,
    pending: Mutex<Option<Handle>>,
    closed: AtomicBool,
}

fn new_instance(name: &str, first: bool) -> io::Result<HANDLE> {
    let w = wide(name);
    let sddl = wide(OWNER_ONLY);
    // SAFETY: the strings are NUL-ended; the descriptor is freed after CreateNamedPipeW copied it.
    unsafe {
        let mut sd: *mut c_void = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) == 0 {
            return Err(io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
        let mut flags = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            flags |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        let h = CreateNamedPipeW(
            w.as_ptr(),
            flags,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | windows_sys::Win32::System::Pipes::PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFSIZE,
            BUFSIZE,
            NMPWAIT_WAIT_FOREVER,
            &sa,
        );
        let e = io::Error::last_os_error();
        LocalFree(sd);
        if h == INVALID_HANDLE_VALUE { Err(e) } else { Ok(h) }
    }
}

impl OwnerListener {
    /// Fails while any process holds the name.
    pub fn new(name: &str) -> io::Result<OwnerListener> {
        let h = new_instance(name, true)?;
        Ok(OwnerListener {
            name: name.to_string(),
            queue: Mutex::new(VecDeque::from([Handle(h)])),
            pending: Mutex::new(None),
            closed: AtomicBool::new(false),
        })
    }

    /// The first handle (for the security check in the tests).
    #[cfg(test)]
    pub(crate) fn first_handle(&self) -> HANDLE {
        self.queue.lock().unwrap().front().map_or(null_mut(), |h| h.0)
    }

    /// Wait for the next client. Fails once `close` ran.
    pub fn accept(&self) -> io::Result<Conn> {
        if self.closed.load(SeqCst) {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        }
        let next = new_instance(&self.name, false)?;
        let h = {
            let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.push_back(Handle(next));
            q.pop_front().expect("a pipe instance").0
        };
        let ev = event()?;
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(Handle(h));
        // SAFETY: ov lives until the connect finished; h is ours.
        let r = unsafe {
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = ev.0;
            let mut n = 0;
            if ConnectNamedPipe(h, &mut ov) != 0 {
                Ok(())
            } else {
                match GetLastError() {
                    ERROR_PIPE_CONNECTED | ERROR_NO_DATA => Ok(()),
                    ERROR_IO_PENDING if GetOverlappedResult(h, &ov, &mut n, 1) != 0 => Ok(()),
                    _ => Err(io::Error::last_os_error()),
                }
            }
        };
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // SAFETY: the event belongs to this call.
        unsafe { CloseHandle(ev.0) };
        let r = r.and_then(|()| if self.closed.load(SeqCst) { Err(io::Error::from(io::ErrorKind::NotConnected)) } else { Ok(()) });
        match r {
            Ok(()) => Conn::new(h, true),
            Err(e) => {
                // SAFETY: h is ours and no Conn owns it.
                unsafe { CloseHandle(h) };
                Err(e)
            }
        }
    }

    /// Stop a waiting `accept` and refuse new ones; the pipe name frees on drop.
    pub fn close(&self) {
        self.closed.store(true, SeqCst);
        if let Some(h) = &*self.pending.lock().unwrap_or_else(|e| e.into_inner()) {
            // SAFETY: the handle is open until accept returns.
            unsafe { CancelIoEx(h.0, null()) };
        }
    }
}

impl Drop for OwnerListener {
    fn drop(&mut self) {
        self.close();
        for h in self.queue.get_mut().unwrap_or_else(|e| e.into_inner()).drain(..) {
            // SAFETY: the queued handles belong to the listener only.
            unsafe { CloseHandle(h.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc_4231() {
        // RFC 4231 test case 2 (the Python handshake uses hmac.new(key, msg, "sha256"))
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        assert_eq!(token_hex(16).len(), 32);
    }
}
