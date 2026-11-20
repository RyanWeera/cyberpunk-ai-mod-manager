//! Stand-in for Cyberpunk2077.exe in tests/e2e_crash.rs. Usage: fake_game clean|crash [mod.dll]
//! Runs 8 s, then exits normally or crashes inside the mod DLL (strchr on address 8, on a native thread).
//! Like the game's own crash reporter, it writes a minidump to %LOCALAPPDATA%\REDEngine\ReportQueue\<id>\.
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::System::Diagnostics::Debug::{
    EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION, MiniDumpWriteDump, SetErrorMode, SetUnhandledExceptionFilter,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Threading::{CreateThread, GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId};

unsafe extern "system" fn reporter(pointers: *const EXCEPTION_POINTERS) -> i32 {
    let id = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() ^ u128::from(std::process::id());
    let d = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("REDEngine/ReportQueue").join(format!("{id:032x}"));
    if std::fs::create_dir_all(&d).is_ok()
        && let Ok(f) = std::fs::File::create(d.join("minidump.dmp"))
    {
        // SAFETY: plain calls; thread id is ours and the pointers come from Windows for this exception.
        let info = unsafe {
            MINIDUMP_EXCEPTION_INFORMATION { ThreadId: GetCurrentThreadId(), ExceptionPointers: pointers as *mut _, ClientPointers: 0 }
        };
        // SAFETY: live process and file handles; 0 = MiniDumpNormal.
        unsafe { MiniDumpWriteDump(GetCurrentProcess(), GetCurrentProcessId(), f.as_raw_handle(), 0, &info, std::ptr::null(), std::ptr::null()) };
    }
    1 // EXCEPTION_EXECUTE_HANDLER: end the process with the exception code
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // SAFETY: plain call; no Windows error dialog.
    unsafe { SetErrorMode(3) };
    std::thread::sleep(Duration::from_secs(8));
    if args.get(1).map(String::as_str) == Some("clean") {
        return;
    }
    let dll: Vec<u16> = args[2].encode_utf16().chain([0]).collect();
    // SAFETY: the filter has the right type; the DLL path is a NUL-ended wide string; strchr(8, ..) is
    // started as a thread on purpose, so it reads address 8 and crashes inside the mod DLL.
    unsafe {
        SetUnhandledExceptionFilter(Some(reporter));
        let h = LoadLibraryW(dll.as_ptr());
        let strchr = GetProcAddress(h, c"strchr".as_ptr().cast()).expect("the DLL has no strchr");
        let start = std::mem::transmute::<unsafe extern "system" fn() -> isize, unsafe extern "system" fn(*mut c_void) -> u32>(strchr);
        CreateThread(std::ptr::null(), 0, Some(start), 8 as *const c_void, 0, std::ptr::null_mut());
    }
    std::thread::sleep(Duration::from_secs(30));
}
