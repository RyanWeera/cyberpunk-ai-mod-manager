//! Small Windows helpers: registry strings (current user or machine) and UTF-16 text.
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
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
