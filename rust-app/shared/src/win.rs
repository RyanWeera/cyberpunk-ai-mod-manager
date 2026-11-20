//! Small Windows helpers: registry strings (current user or machine), UTF-16 text, and windows off the taskbar.
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND, LPARAM};
use windows_sys::core::BOOL;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetWindowLongPtrW, GetWindowThreadProcessId, IsWindowVisible, SW_HIDE, SW_SHOWNOACTIVATE,
    SetWindowLongPtrW, ShowWindowAsync, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegGetValueW, RegSetValueExW,
};

/// A registry root key.
#[derive(Clone, Copy)]
pub enum Root {
    CurrentUser,
    LocalMachine,
}

pub use Root::{CurrentUser as HKCU, LocalMachine as HKLM};

fn hkey(root: Root) -> HKEY {
    match root {
        Root::CurrentUser => HKEY_CURRENT_USER,
        Root::LocalMachine => HKEY_LOCAL_MACHINE,
    }
}

/// Text as a NUL-terminated UTF-16 string for Win32 calls.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A REG_SZ value ("" names the key's default value), or None when the key or value is missing.
pub fn reg_get(root: Root, subkey: &str, name: &str) -> Option<String> {
    let (root, sub, name) = (hkey(root), wide(subkey), wide(name));
    let mut size = 0u32;
    // SAFETY: first call only asks for the size; pointers are valid NUL-terminated strings.
    let rc = unsafe { RegGetValueW(root, sub.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) };
    if rc != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    // SAFETY: buf holds `size` bytes.
    let rc = unsafe { RegGetValueW(root, sub.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size) };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Create the key when needed and set a REG_SZ value ("" = the default value).
pub fn reg_set(root: Root, subkey: &str, name: &str, value: &str) -> std::io::Result<()> {
    let (root, sub, name, data) = (hkey(root), wide(subkey), wide(name), wide(value));
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: valid strings; key receives the handle and is closed below.
    let rc = unsafe {
        RegCreateKeyExW(root, sub.as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_WRITE, std::ptr::null(), &mut key, std::ptr::null_mut())
    };
    if rc != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    // SAFETY: data is a NUL-terminated UTF-16 buffer of the given byte length.
    let rc = unsafe { RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32) };
    // SAFETY: key is open.
    unsafe { RegCloseKey(key) };
    if rc != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

unsafe extern "system" fn each_window(hwnd: HWND, lp: LPARAM) -> BOOL {
    // SAFETY: `lp` is the (pid, windows) pair that `to_background` passes, alive during EnumWindows.
    let (pid, wins) = unsafe { &mut *(lp as *mut (u32, Vec<HWND>)) };
    let mut owner = 0u32;
    // SAFETY: a window handle from EnumWindows.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut owner);
        if owner == *pid && IsWindowVisible(hwnd) != 0 {
            wins.push(hwnd);
        }
    }
    1
}

/// Takes the visible windows of process `pid` off the taskbar and Alt+Tab (a tool window). They stay open, so
/// their pages keep running. Returns how many visible windows the process has.
pub fn to_background(pid: u32) -> usize {
    let mut st: (u32, Vec<HWND>) = (pid, vec![]);
    // SAFETY: the callback only uses `st` during the call; the handles are windows EnumWindows just gave.
    unsafe {
        EnumWindows(Some(each_window), &mut st as *mut _ as LPARAM);
        for &w in &st.1 {
            let ex = GetWindowLongPtrW(w, GWL_EXSTYLE);
            if ex & WS_EX_TOOLWINDOW as isize == 0 {
                // the taskbar reads the style when a window shows, so hide it, change it, show it again
                ShowWindowAsync(w, SW_HIDE);
                SetWindowLongPtrW(w, GWL_EXSTYLE, (ex | WS_EX_TOOLWINDOW as isize) & !(WS_EX_APPWINDOW as isize));
                ShowWindowAsync(w, SW_SHOWNOACTIVATE);
            }
        }
    }
    st.1.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_value_is_none() {
        assert_eq!(reg_get(HKCU, r"Software\CP2077MM-test-does-not-exist", "x"), None);
    }

    #[test]
    fn set_then_get() {
        let key = r"Software\CP2077MM-test-key";
        reg_set(HKCU, key, "", "hello é").unwrap();
        assert_eq!(reg_get(HKCU, key, "").as_deref(), Some("hello é"));
        let w = wide(key);
        // SAFETY: valid string; removes the test key.
        unsafe { windows_sys::Win32::System::Registry::RegDeleteKeyW(HKEY_CURRENT_USER, w.as_ptr()) };
    }
}
