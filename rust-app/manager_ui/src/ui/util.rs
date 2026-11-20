//! Small text helpers of the window: numbers, dates, local time and the clipboard.

use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

/// The egui image URI of a local file. It needs three slashes: with two, `C:` is read as a network host.
pub fn file_uri(p: &std::path::Path) -> String {
    format!("file:///{}", p.to_string_lossy().replace('\\', "/"))
}

/// 20381596 -> "20.4M", like a store page.
pub fn short(n: f64) -> String {
    for (size, unit) in [(1e9, "B"), (1e6, "M"), (1e3, "K")] {
        if n >= size {
            let s = format!("{:.1}", n / size);
            return format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s));
        }
    }
    if n.fract() == 0.0 { format!("{}", n as i64) } else { n.to_string() }
}

/// 1234567 -> "1,234,567" (Python's `{n:,}`).
pub fn commas(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Days since 1970-01-01 for a calendar date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// "2026-11-17T10:00:00Z" (or "+hh:mm", or no zone = UTC, or a plain date) -> unix seconds.
pub fn parse_iso(s: &str) -> Option<i64> {
    let num = |a: usize, b: usize| s.get(a..b).filter(|t| t.bytes().all(|c| c.is_ascii_digit())).and_then(|t| t.parse::<i64>().ok());
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    if s.get(4..5) != Some("-") || s.get(7..8) != Some("-") || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut t = days_from_civil(y, mo, d) * 86_400;
    if s.len() == 10 {
        return Some(t);
    }
    if !matches!(s.get(10..11), Some("T" | " ")) || s.get(13..14) != Some(":") {
        return None;
    }
    let (h, mi) = (num(11, 13)?, num(14, 16)?);
    let mut rest = &s[16..];
    let mut sec = 0;
    if let Some(r) = rest.strip_prefix(':') {
        sec = r.get(..2).filter(|t| t.bytes().all(|c| c.is_ascii_digit()))?.parse().ok()?;
        rest = &r[2..];
        if let Some(f) = rest.strip_prefix('.') {
            let n = f.bytes().take_while(u8::is_ascii_digit).count();
            if n == 0 {
                return None;
            }
            rest = &f[n..];
        }
    }
    if h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    t += h * 3600 + mi * 60 + sec;
    match rest {
        "" | "Z" => Some(t),
        z if z.len() == 6 && matches!(&z[..1], "+" | "-") && &z[3..4] == ":" => {
            let off = z[1..3].parse::<i64>().ok()? * 3600 + z[4..6].parse::<i64>().ok()? * 60;
            Some(if &z[..1] == "+" { t - off } else { t + off })
        }
        _ => None,
    }
}

/// "2026-11-17T10:00:00Z" -> "3 days ago"; "" for a date that cannot be read.
pub fn ago(iso: &str, now: Option<i64>) -> String {
    let Some(t) = parse_iso(iso) else { return String::new() };
    let days = (now.unwrap_or_else(self::now) - t).div_euclid(86_400);
    for (size, unit) in [(365, "year"), (30, "month"), (1, "day")] {
        if days >= size {
            let n = days / size;
            return format!("{n} {unit}{} ago", if n > 1 { "s" } else { "" });
        }
    }
    "today".into()
}

/// Python `time.strftime(fmt, time.localtime(t))` for %Y %m %d %H %M %S %b.
pub fn strftime(fmt: &str, t: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let ft = (t * 10_000_000 + 116_444_736_000_000_000) as u64;
    let ft = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    let (mut utc, mut st) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    // SAFETY: plain value structs on the stack.
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc);
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut st) == 0 {
            st = utc;
        }
    }
    let mut out = String::new();
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('Y') => out += &format!("{:04}", st.wYear),
            Some('m') => out += &format!("{:02}", st.wMonth),
            Some('d') => out += &format!("{:02}", st.wDay),
            Some('H') => out += &format!("{:02}", st.wHour),
            Some('M') => out += &format!("{:02}", st.wMinute),
            Some('S') => out += &format!("{:02}", st.wSecond),
            Some('b') => out += MONTHS[(st.wMonth.clamp(1, 12) - 1) as usize],
            Some(o) => {
                out.push('%');
                out.push(o);
            }
            None => out.push('%'),
        }
    }
    out
}

/// The text on the Windows clipboard, or None when it holds no text.
pub fn clipboard_text() -> Option<String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    // SAFETY: the clipboard is opened and closed here; the scan stops at a NUL or at the end of the locked buffer.
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h = GetClipboardData(CF_UNICODETEXT);
        let mut out = None;
        if !h.is_null() {
            let p = GlobalLock(h) as *const u16;
            if !p.is_null() {
                let max = GlobalSize(h) / 2;
                let mut n = 0;
                while n < max && *p.add(n) != 0 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                GlobalUnlock(h);
            }
        }
        CloseClipboard();
        out.filter(|t| !t.is_empty())
    }
}
