//! Watch for the game process, and after it exits collect crash evidence: its exit code, Windows crash events, and
//! REDEngine crash reports (whose minidump names the module that crashed). While the game runs, a window that stays
//! "Not responding" gets a snapshot of every thread, so a freeze can be explained too.
use crate::doctor::{mtime, now, rglob, strftime};
use regex::Regex;
use serde_json::{Map, Value, json};
use shared::sys32;
use shared::win::wide;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs;
use std::io::{self, Read};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
use windows_sys::Win32::Globalization::MultiByteToWideChar;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LoadLibraryExW,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, WaitForSingleObject};
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsHungAppWindow, IsWindowVisible};
use windows_sys::core::BOOL;

pub const EXE: &str = "Cyberpunk2077.exe";
const NO_WINDOW: u32 = 0x0800_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const QUERY_LIMITED: u32 = 0x1000;
pub const WAIT_TIMEOUT: u32 = 0x102;
/// Seconds a window stays "Not responding" (Windows says so after 5 s) before the snapshot: 25 s in all.
pub const HANG_AFTER: f64 = 20.0;
const VM_READ: u32 = 0x0010;
const QUERY_INFO: u32 = 0x0400;
const TRIES: usize = 4;
const LAG: f64 = 10.0;

/// SYMBOL_INFOW with room for a 255-character name.
#[repr(C)]
struct SymbolInfo {
    size_of_struct: u32,
    type_index: u32,
    reserved: [u64; 2],
    index: u32,
    size: u32,
    mod_base: u64,
    flags: u32,
    value: u64,
    address: u64,
    register: u32,
    scope: u32,
    tag: u32,
    name_len: u32,
    max_name_len: u32,
    name: [u16; 256],
}

type Farproc = unsafe extern "system" fn() -> isize;
type MiniDumpWriteDump = unsafe extern "system" fn(HANDLE, u32, HANDLE, u32, *const c_void, *const c_void, *const c_void) -> BOOL;
type SymSetOptions = unsafe extern "system" fn(u32) -> u32;
type SymInitializeW = unsafe extern "system" fn(HANDLE, *const u16, BOOL) -> BOOL;
type SymLoadModuleExW = unsafe extern "system" fn(HANDLE, HANDLE, *const u16, *const u16, u64, u32, *const c_void, u32) -> u64;
type SymFromAddrW = unsafe extern "system" fn(HANDLE, u64, *mut u64, *mut SymbolInfo) -> BOOL;
type SymCleanup = unsafe extern "system" fn(HANDLE) -> BOOL;

/// dbghelp.dll from System32 by full path: Windows would look next to our exe first.
fn dbghelp() -> Option<usize> {
    static DLL: OnceLock<usize> = OnceLock::new();
    let h = *DLL.get_or_init(|| {
        let p = wide(&sys32().join("dbghelp.dll").to_string_lossy());
        // SAFETY: a NUL-terminated path; the module stays loaded for the life of the process.
        unsafe { LoadLibraryExW(p.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) as usize }
    });
    (h != 0).then_some(h)
}

/// The address of a dbghelp function (`name` ends with NUL).
fn proc(name: &[u8]) -> Option<Farproc> {
    // SAFETY: a loaded module and a NUL-terminated name.
    unsafe { GetProcAddress(dbghelp()? as *mut c_void, name.as_ptr()) }
}

unsafe extern "system" fn each_window(hwnd: HWND, lp: LPARAM) -> BOOL {
    // SAFETY: `lp` is the (pid, windows) pair `hung` passes, alive during EnumWindows.
    let (pid, wins) = unsafe { &mut *(lp as *mut (u32, Vec<usize>)) };
    let mut owner = 0u32;
    // SAFETY: a window handle from EnumWindows.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut owner);
        if owner == *pid && IsWindowVisible(hwnd) != 0 {
            wins.push(hwnd as usize);
        }
    }
    1
}

/// True when a visible window of process `pid` is "Not responding".
pub fn hung(pid: u32) -> bool {
    let mut st: (u32, Vec<usize>) = (pid, vec![]);
    // SAFETY: the callback only uses `st` during the call.
    unsafe {
        EnumWindows(Some(each_window), &mut st as *mut _ as LPARAM);
        st.1.iter().any(|&w| IsHungAppWindow(w as HWND) != 0)
    }
}

fn has_name(p: &Path, f: impl Fn(&str) -> bool) -> bool {
    p.file_name().is_some_and(|n| f(&n.to_string_lossy().to_lowercase()))
}

/// Snapshot (threads, their stacks, modules) of running process `pid` as a minidump in `folder`: its path, or None.
/// Only the newest `keep` snapshots stay.
pub fn write_dump(pid: u32, folder: &Path, keep: usize) -> Option<PathBuf> {
    // SAFETY: plain call; the handle is closed below.
    let h = unsafe { OpenProcess(QUERY_INFO | VM_READ, 0, pid) };
    if h.is_null() {
        return None; // e.g. the game runs as administrator and this app does not
    }
    let run = || -> io::Result<Option<PathBuf>> {
        fs::create_dir_all(folder)?;
        let path = folder.join(strftime("freeze-%Y%m%d-%H%M%S.dmp", now()));
        // SAFETY: the function type matches MiniDumpWriteDump.
        let Some(write) = proc(b"MiniDumpWriteDump\0").map(|f| unsafe { std::mem::transmute::<Farproc, MiniDumpWriteDump>(f) }) else {
            return Ok(None);
        };
        let ok = {
            let f = fs::File::create(&path)?;
            // SAFETY: live process and file handles; 0 = MiniDumpNormal.
            unsafe { write(h, pid, f.as_raw_handle(), 0, std::ptr::null(), std::ptr::null(), std::ptr::null()) != 0 }
        };
        if !ok {
            let _ = fs::remove_file(&path);
            return Ok(None);
        }
        let mut old: Vec<PathBuf> = fs::read_dir(folder)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| has_name(p, |n| n.starts_with("freeze-") && n.ends_with(".dmp")))
            .collect();
        old.sort_by_key(|p| p.to_string_lossy().to_lowercase());
        if keep > 0 && old.len() > keep {
            for p in &old[..old.len() - keep] {
                let _ = fs::remove_file(p);
            }
        }
        Ok(Some(path))
    };
    let out = run().ok().flatten();
    // SAFETY: the handle from OpenProcess above.
    unsafe { CloseHandle(h) };
    out
}

/// Process id of the running game, or None.
pub fn find_pid() -> Option<u32> {
    // SAFETY: the snapshot handle is closed below; the entry is a plain struct with its size set.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap as isize == -1 {
            return None;
        }
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut found = None;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            if String::from_utf16_lossy(&e.szExeFile[..n]).eq_ignore_ascii_case(EXE) {
                found = Some(e.th32ProcessID);
                break;
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
        found
    }
}

/// The Windows calls `watch` makes, so tests can play a game process.
pub(crate) trait Os {
    fn find_pid(&self) -> Option<u32>;
    /// A process handle, or None when Windows will not let us open it.
    fn open(&self, pid: u32) -> Option<usize>;
    fn wait(&self, h: usize, ms: u32) -> u32;
    fn exit_code(&self, h: usize) -> Option<u32>;
    fn close(&self, h: usize);
    fn hung(&self, pid: u32) -> bool;
    fn write_dump(&self, pid: u32, folder: &Path) -> Option<PathBuf>;
    fn now(&self) -> f64;
}

struct Win;

impl Os for Win {
    fn find_pid(&self) -> Option<u32> {
        find_pid()
    }
    fn open(&self, pid: u32) -> Option<usize> {
        // SAFETY: plain call; `close` closes the handle.
        let h = unsafe { OpenProcess(SYNCHRONIZE | QUERY_LIMITED, 0, pid) };
        (!h.is_null()).then_some(h as usize)
    }
    fn wait(&self, h: usize, ms: u32) -> u32 {
        // SAFETY: a handle from `open`.
        unsafe { WaitForSingleObject(h as HANDLE, ms) }
    }
    fn exit_code(&self, h: usize) -> Option<u32> {
        let mut c = 0u32;
        // SAFETY: a handle from `open`.
        (unsafe { GetExitCodeProcess(h as HANDLE, &mut c) } != 0).then_some(c)
    }
    fn close(&self, h: usize) {
        // SAFETY: a handle from `open`, closed once.
        unsafe { CloseHandle(h as HANDLE) };
    }
    fn hung(&self, pid: u32) -> bool {
        hung(pid)
    }
    fn write_dump(&self, pid: u32, folder: &Path) -> Option<PathBuf> {
        write_dump(pid, folder, 5)
    }
    fn now(&self) -> f64 {
        now()
    }
}

/// Wait up to `secs` for `stop`: true when it is set (Python's `Event.wait`).
fn wait_stop(stop: &AtomicBool, secs: f64) -> bool {
    let end = Instant::now() + Duration::from_secs_f64(secs.max(0.0));
    loop {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// Call on_start(t) when the game starts and on_exit(t_started, exit_code, freeze) when it stops, until `stop` is set.
/// exit_code is None when Windows would not let us open the process. freeze is the snapshot (in hang_dir) of a freeze
/// the game never came back from, or None.
pub fn watch(
    mut on_start: impl FnMut(f64),
    mut on_exit: impl FnMut(f64, Option<u32>, Option<PathBuf>),
    stop: &AtomicBool,
    interval: f64,
    hang_dir: Option<&Path>,
) {
    watch_with(&Win, &mut on_start, &mut on_exit, stop, interval, hang_dir)
}

pub(crate) fn watch_with(
    os: &dyn Os,
    on_start: &mut dyn FnMut(f64),
    on_exit: &mut dyn FnMut(f64, Option<u32>, Option<PathBuf>),
    stop: &AtomicBool,
    interval: f64,
    hang_dir: Option<&Path>,
) {
    while !wait_stop(stop, interval) {
        // one bad cycle (a panic in a callback) must not end crash watching
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let Some(pid) = os.find_pid() else { return };
            let h = os.open(pid);
            let started = os.now();
            let (mut code, mut hung_since) = (None, None::<f64>);
            // None: no snapshot yet; Some(None): it failed, no second try in this freeze
            let mut freeze: Option<Option<PathBuf>> = None;
            on_start(started);
            if let Some(h) = h {
                while os.wait(h, 1000) == WAIT_TIMEOUT && !stop.load(Ordering::SeqCst) {
                    let Some(dir) = hang_dir else { continue };
                    if !os.hung(pid) {
                        hung_since = None;
                        // it came back: a long load, not a freeze
                        let gone = match &freeze {
                            Some(Some(p)) => fs::remove_file(p).is_ok() || !p.exists(),
                            _ => true,
                        };
                        if gone {
                            freeze = None;
                        }
                    } else if let Some(t) = hung_since {
                        if freeze.is_none() && os.now() - t >= HANG_AFTER {
                            freeze = Some(os.write_dump(pid, dir));
                        }
                    } else {
                        hung_since = Some(os.now());
                    }
                }
                code = os.exit_code(h);
                os.close(h);
                if let (Some(Some(p)), Some(0)) = (&freeze, code) {
                    // a clean exit: it was a slow quit, not a freeze
                    let _ = fs::remove_file(p);
                    freeze = None;
                }
            } else {
                while os.find_pid() == Some(pid) && !wait_stop(stop, interval) {}
            }
            on_exit(started, code, freeze.flatten());
        }));
    }
}

fn re(p: &str) -> Regex {
    Regex::new(p).expect("regex")
}

static MODULE_PATH: LazyLock<Regex> = LazyLock::new(|| re(r"Faulting module path:\s*(.+)"));
static START: LazyLock<Regex> = LazyLock::new(|| re(r"(?:application start time|Start Time):\s*(?:0x)?([0-9a-fA-F]{8,})"));
static OFFSET: LazyLock<Regex> = LazyLock::new(|| re(r"Fault offset:\s*0x([0-9a-fA-F]+)"));

/// [(faulting module path or "" for hangs, process start as unix time or None, fault offset or None)] for game
/// crash events in `wevtutil /f:text` output.
pub fn parse_events(text: &str) -> Vec<(String, Option<f64>, Option<u64>)> {
    let exe = EXE.to_lowercase();
    let mut out = vec![];
    for block in text.split("Event[").skip(1) {
        let low = block.to_lowercase();
        if !low.contains(&format!("application name: {exe}")) && !low.contains(&format!("program {exe}")) {
            continue;
        }
        let module = MODULE_PATH.captures(block).map(|c| c[1].trim().to_string()).unwrap_or_default();
        // FILETIME -> unix
        let started = START
            .captures(block)
            .and_then(|c| u128::from_str_radix(&c[1], 16).ok())
            .map(|v| (v as i128 - 116_444_736_000_000_000) as f64 / 1e7);
        let offset = OFFSET.captures(block).and_then(|c| u64::from_str_radix(&c[1], 16).ok());
        out.push((module, started, offset));
    }
    out
}

/// Function name at `addr` from the .pdb file next to a module, or None. Mods rarely ship one.
pub fn symbol(path: &str, base: u64, size: u32, addr: u64) -> Option<String> {
    // "C:" only: no \\host\share paths
    if path.chars().nth(1) != Some(':') || !Path::new(path).with_extension("pdb").is_file() {
        return None;
    }
    // SAFETY: each function type matches its dbghelp export.
    let (set, init, load, from, clean) = unsafe {
        (
            std::mem::transmute::<Farproc, SymSetOptions>(proc(b"SymSetOptions\0")?),
            std::mem::transmute::<Farproc, SymInitializeW>(proc(b"SymInitializeW\0")?),
            std::mem::transmute::<Farproc, SymLoadModuleExW>(proc(b"SymLoadModuleExW\0")?),
            std::mem::transmute::<Farproc, SymFromAddrW>(proc(b"SymFromAddrW\0")?),
            std::mem::transmute::<Farproc, SymCleanup>(proc(b"SymCleanup\0")?),
        )
    };
    let h = 0x5CB0 as HANDLE; // any unique value: no live process is read
    let dir = wide(&Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
    let file = wide(path);
    // SAFETY: NUL-terminated strings and a zeroed SYMBOL_INFOW with its sizes set.
    unsafe {
        set(0x2 | 0x200 | 0x1000 | 0x80000); // undecorate names, no error boxes, ignore _NT_SYMBOL_PATH, no prompts
        if init(h, dir.as_ptr(), 0) == 0 {
            return None; // search only the mod's folder, never a symbol server
        }
        let mut info: SymbolInfo = std::mem::zeroed();
        info.size_of_struct = 88;
        info.max_name_len = 255;
        let mut disp = 0u64;
        let mut name = None;
        // 0x200: export only
        if load(h, std::ptr::null_mut(), file.as_ptr(), std::ptr::null(), base, size, std::ptr::null(), 0) != 0
            && from(h, addr, &mut disp, &mut info) != 0
            && info.flags & 0x200 == 0
        {
            let n = info.name.iter().position(|&c| c == 0).unwrap_or(info.name.len());
            name = Some(String::from_utf16_lossy(&info.name[..n]));
        }
        clean(h);
        name
    }
}

/// The file name of a Windows or posix path.
fn base_name(p: &str) -> &str {
    p.rsplit(['\\', '/']).find(|s| !s.is_empty()).unwrap_or("")
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off.checked_add(4)?)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off.checked_add(8)?)?.try_into().ok()?))
}

/// From a minidump (.dmp):
/// module: path of the module the crash happened in (None for a freeze snapshot, which has no crash)
/// code: exception code
/// stack: modules on the crashing thread's stack (on the main thread in a freeze snapshot), newest call first
/// others: modules on the other threads' stacks (freeze snapshots only)
/// places: {module path: "Name.dll+0xOFFSET (function)"} where the crash happened and where each stack module was found
/// fault: crash ID "Name.dll+0xOFFSET" of the crashing instruction, or None
/// or None when the file is no minidump.
pub fn read_dump(path: &Path) -> Option<Value> {
    let b = fs::read(path).ok()?;
    if b.get(..4)? != b"MDMP" {
        return None;
    }
    let (n, rva) = (u32_at(&b, 8)? as usize, u32_at(&b, 12)? as usize);
    let mut streams = HashMap::new();
    for i in 0..n {
        let at = rva + 12 * i;
        streams.insert(u32_at(&b, at)?, u32_at(&b, at + 8)? as usize);
    }
    // 6 = exception, 4 = module list, 3 = thread list
    let ml = *streams.get(&4)?;
    if !streams.contains_key(&3) && !streams.contains_key(&6) {
        return None;
    }
    let (mut tid, mut code, mut addr) = (None, None, None);
    if let Some(&e) = streams.get(&6) {
        // ThreadId, pad, code, flags, record, address
        tid = Some(u32_at(&b, e)?);
        code = Some(u32_at(&b, e + 8)?);
        addr = Some(u64_at(&b, e + 24)?);
    }
    let mut mods: Vec<(u64, u64, String)> = vec![];
    for i in 0..u32_at(&b, ml)? as usize {
        let at = ml + 4 + 108 * i;
        let (base, size, name_rva) = (u64_at(&b, at)?, u32_at(&b, at + 8)?, u32_at(&b, at + 20)? as usize);
        let len = u32_at(&b, name_rva)? as usize;
        let raw = b.get(name_rva + 4..).unwrap_or(&[]);
        let raw = &raw[..len.min(raw.len())];
        if raw.len() % 2 != 0 {
            return None;
        }
        let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        mods.push((base, base.saturating_add(size as u64), String::from_utf16(&units).ok()?));
    }
    mods.sort();
    let mut places = Map::new();
    let mut at = |a: u64| -> Option<String> {
        let i = mods.partition_point(|m| m.0 <= a).checked_sub(1)?;
        let m = &mods[i];
        if a >= m.1 || m.2.is_empty() {
            return None;
        }
        if !places.contains_key(&m.2) {
            let sym = symbol(&m.2, m.0, (m.1 - m.0) as u32, a).map(|s| format!(" ({s})")).unwrap_or_default();
            places.insert(m.2.clone(), json!(format!("{}+0x{:X}{sym}", base_name(&m.2), a - m.0)));
        }
        Some(m.2.clone())
    };
    let module = addr.and_then(&mut at);
    let mut threads: Vec<(u32, usize, usize)> = vec![];
    if let Some(&tl) = streams.get(&3) {
        for i in 0..(u32_at(&b, tl)? as usize).min(1024) {
            let e = tl + 4 + 48 * i;
            u64_at(&b, e + 24)?; // the whole 40-byte entry must be there
            threads.push((u32_at(&b, e)?, u32_at(&b, e + 32)? as usize, u32_at(&b, e + 36)? as usize));
        }
    }
    if let Some(tid) = tid {
        threads.retain(|t| t.0 == tid); // a crash: only the crashing thread matters
    }
    // ponytail: every stack slot that points into a module counts, so an old return address can show up too;
    // walk the unwind data if this lists the wrong mods. The first thread of a snapshot is taken as the main thread.
    let mut stacks: Vec<Vec<String>> = vec![];
    for &(_, size, rva) in &threads {
        let words = b.get(rva..rva.checked_add(size / 8 * 8)?)?;
        let mut hits: Vec<String> = vec![];
        for w in words.as_chunks::<8>().0 {
            if let Some(m) = at(u64::from_le_bytes(*w))
                && !hits.contains(&m)
            {
                hits.push(m);
            }
        }
        stacks.push(hits);
    }
    let stack = stacks.first().cloned().unwrap_or_default();
    let mut others: Vec<String> = vec![];
    for m in stacks.iter().skip(1).flatten() {
        if !stack.contains(m) && !others.contains(m) {
            others.push(m.clone());
        }
    }
    let fault = module
        .as_ref()
        .and_then(|m| places.get(m))
        .and_then(Value::as_str)
        .map(|p| p.split(" (").next().unwrap_or("").to_string());
    Some(json!({"module": module, "code": code, "stack": stack, "others": others, "places": places, "fault": fault}))
}

/// Path of the module the crash happened in, read from a minidump (.dmp), or None.
pub fn dump_module(path: &Path) -> Option<String> {
    read_dump(path)?.get("module")?.as_str().map(String::from)
}

/// Short crash ID, the same for every crash at the same instruction: "Bad.dll+0x10".
pub fn fault_id(module: &str, offset: u64) -> String {
    format!("{}+0x{offset:X}", base_name(module))
}

/// Text in the ANSI code page (what wevtutil writes to a pipe) as a string.
fn ansi(b: &[u8]) -> String {
    if b.is_empty() || b.len() > i32::MAX as usize {
        return String::new();
    }
    // SAFETY: lengths match the buffers.
    unsafe {
        let n = MultiByteToWideChar(0, 0, b.as_ptr(), b.len() as i32, std::ptr::null_mut(), 0);
        let mut w = vec![0u16; n.max(0) as usize];
        let n = MultiByteToWideChar(0, 0, b.as_ptr(), b.len() as i32, w.as_mut_ptr(), w.len() as i32);
        String::from_utf16_lossy(&w[..n.max(0) as usize])
    }
}

/// Output of `wevtutil.exe args`, or "" when it fails or runs over 60 seconds.
fn wevtutil(args: &[String]) -> String {
    let Ok(mut child) = Command::new(sys32().join("wevtutil.exe"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(NO_WINDOW)
        .spawn()
    else {
        return String::new();
    };
    let Some(mut out) = child.stdout.take() else { return String::new() };
    let reader = std::thread::spawn(move || {
        let mut buf = vec![];
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(60) => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return String::new();
            }
        }
    }
    ansi(&reader.join().unwrap_or_default())
}

/// `since` is when polling saw the game start (up to one poll late); events of older game processes are ignored.
/// Windows writes its crash event a moment after the process ends, so look a few times when the exit code says crash.
pub fn crash_evidence(since: f64, exit_code: Option<u32>) -> Value {
    // NTSTATUS error, e.g. 0xC0000005 access violation
    let bad_exit = exit_code.is_some_and(|c| c >= 0xC000_0000);
    let rq = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join(r"REDEngine\ReportQueue");
    let mut attempt = 0;
    loop {
        let ms = ((now() - since) * 1000.0) as i64 + 60_000;
        let q = format!(
            "*[System[(Provider[@Name='Application Error'] or Provider[@Name='Application Hang']) and TimeCreated[timediff(@SystemTime) <= {ms}]]]"
        );
        let args: Vec<String> = ["qe", "Application", &format!("/q:{q}"), "/f:text", "/rd:true", "/c:20"].map(String::from).into();
        let events: Vec<(String, Option<u64>)> = parse_events(&wevtutil(&args))
            .into_iter()
            .filter(|e| e.1.is_none_or(|s| s >= since - LAG))
            .map(|e| (e.0, e.2))
            .collect();
        let reports: Vec<PathBuf> = if rq.is_dir() {
            // the crash reporter can delete a report while we look
            fs::read_dir(&rq).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| mtime(p).unwrap_or(0.0) >= since).collect()
        } else {
            vec![]
        };
        let dump = reports
            .iter()
            .flat_map(|r| rglob(r))
            .filter(|f| has_name(f, |n| n.ends_with(".dmp")))
            .find_map(|f| read_dump(&f))
            .unwrap_or_else(|| json!({}));
        let dump_mod = dump.get("module").and_then(Value::as_str);
        let hit = events.iter().find(|e| !e.0.is_empty());
        let module = hit.map(|h| h.0.as_str()).or(dump_mod);
        let fault = match hit {
            Some((m, Some(off))) => Some(fault_id(m, *off)),
            _ if module == dump_mod => dump.get("fault").and_then(Value::as_str).map(String::from),
            _ => None,
        };
        let crashed = bad_exit || !events.is_empty() || !reports.is_empty();
        if module.is_some() || attempt == TRIES - 1 || (!crashed && attempt >= 1) {
            let code = dump.get("code").filter(|c| crate::doctor::truthy(c)).cloned().unwrap_or_else(|| json!(exit_code.filter(|_| bad_exit)));
            return json!({"crashed": crashed, "module": module, "report": reports.first().map(|r| r.to_string_lossy().into_owned()),
                          "exit_code": exit_code, "code": code, "stack": dump.get("stack").cloned().unwrap_or_else(|| json!([])),
                          "places": dump.get("places").cloned().unwrap_or_else(|| json!({})), "fault": fault});
        }
        attempt += 1;
        std::thread::sleep(Duration::from_secs_f64(2.5));
    }
}
