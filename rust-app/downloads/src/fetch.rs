//! HTTP helpers, file downloads with progress, and GitHub releases for the core frameworks.
//! Every HTTP request in the workspace goes through `send` (or `download`), so tests can replace the network
//! with `set_mock`.
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, Instant};

pub use shared::{FRAMEWORKS, fw_by_nexus, fw_by_repo};

// The crate version is the workspace version, the same as `shared::VERSION`.
pub const UA: [(&str, &str); 3] = [
    ("User-Agent", concat!("CP2077-AI-Mod-Manager/", env!("CARGO_PKG_VERSION"))),
    ("Application-Name", "CP2077 AI Mod Manager"),
    ("Application-Version", shared::VERSION),
];

pub const APP_REPO: &str = "RyanWeera/cyberpunk-ai-mod-manager";

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout_secs: u64,
    /// How many redirects to follow; 0 returns the 3xx response so the caller can read `Location`.
    pub redirects: u32,
}

impl Request {
    /// A GET with a 30-second timeout that follows up to 10 redirects (like Python's urllib).
    pub fn get(url: &str) -> Request {
        Request { method: "GET".into(), url: url.into(), headers: vec![], body: None, timeout_secs: 30, redirects: 10 }
    }

    pub fn post(url: &str, body: Vec<u8>) -> Request {
        Request { method: "POST".into(), body: Some(body), ..Request::get(url) }
    }

    pub fn header(mut self, name: &str, value: &str) -> Request {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn timeout(mut self, secs: u64) -> Request {
        self.timeout_secs = secs;
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct Response {
    pub status: u16,
    /// The final URL, after redirects.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// The first header with this name (any case).
    pub fn header(&self, name: &str) -> Option<&str> {
        header_of(&self.headers, name)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Result<Value, String> {
        serde_json::from_slice(&self.body).map_err(|e| format!("not JSON: {e}"))
    }
}

fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

pub type Mock = Arc<dyn Fn(&Request) -> Result<Response, String> + Send + Sync>;

static MOCK: RwLock<Option<Mock>> = RwLock::new(None);

/// Tests: answer every request with `mock` (nothing reaches the network), or `None` for the real network.
pub fn set_mock(mock: Option<Mock>) {
    *MOCK.write().unwrap_or_else(|e| e.into_inner()) = mock;
}

/// A response whose body is still to be read (downloads stream to disk).
struct Stream {
    status: u16,
    url: String,
    headers: Vec<(String, String)>,
    body: Box<dyn Read>,
}

fn open(req: &Request) -> Result<Stream, String> {
    let mock = MOCK.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(mock) = mock {
        let r = mock(req)?;
        return Ok(Stream { status: r.status, url: r.url, headers: r.headers, body: Box::new(Cursor::new(r.body)) });
    }
    // Like Python's socket timeout: the limit is per connect, read and write, not for the whole transfer.
    let t = Some(Duration::from_secs(req.timeout_secs));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(req.redirects)
        .timeout_connect(t)
        .timeout_per_read(t)
        .timeout_per_write(t)
        .build()
        .into();
    let mut b = ureq::http::Request::builder().method(req.method.as_str()).uri(req.url.as_str());
    for (k, v) in &req.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    let resp = match &req.body {
        Some(body) => agent.run(b.body(body.clone()).map_err(|e| e.to_string())?),
        // Like Python's http.client: PUT, POST and PATCH always send a length. The browser's DevTools server
        // closes the connection on a PUT without one.
        None if matches!(req.method.as_str(), "PUT" | "POST" | "PATCH") => agent.run(b.body(Vec::<u8>::new()).map_err(|e| e.to_string())?),
        None => agent.run(b.body(()).map_err(|e| e.to_string())?),
    }
    .map_err(|e| e.to_string())?;
    use ureq::ResponseExt;
    let url = resp.get_uri().to_string();
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    let body = resp.into_body().into_with_config().limit(u64::MAX).reader();
    Ok(Stream { status, url, headers, body: Box::new(body) })
}

/// Send one request. Any HTTP status comes back as a `Response`; only network failures are an `Err`.
pub fn send(req: &Request) -> Result<Response, String> {
    let mut s = open(req)?;
    let mut body = vec![];
    s.body.read_to_end(&mut body).map_err(|e| e.to_string())?;
    Ok(Response { status: s.status, url: s.url, headers: s.headers, body })
}

/// `headers` over `base`, a later name replacing an earlier one in any case.
fn merged(base: &[(&str, &str)], extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for (k, v) in base.iter().chain(extra) {
        out.retain(|(o, _)| !o.eq_ignore_ascii_case(k));
        out.push((k.to_string(), v.to_string()));
    }
    out
}

/// GET `url` (POST when `body` is given) and parse the JSON answer. A non-2xx status is an error.
pub fn http_json(url: &str, body: Option<&Value>, headers: &[(&str, &str)], timeout_secs: u64) -> Result<Value, String> {
    let mut base = UA.to_vec();
    base.push(("Content-Type", "application/json"));
    let req = Request {
        method: if body.is_some() { "POST" } else { "GET" }.into(),
        url: url.into(),
        headers: merged(&base, headers),
        body: body.map(|b| b.to_string().into_bytes()),
        timeout_secs,
        redirects: 10,
    };
    let r = send(&req)?;
    if !(200..300).contains(&r.status) {
        return Err(format!("HTTP Error {}", r.status));
    }
    r.json()
}

static NOT_WINDOWS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)pdb|symbol|mac|darwin|linux").unwrap());

/// The Windows archives of a GitHub release (no debug symbols, no other systems).
pub(crate) fn windows_assets(rel: &Value) -> Vec<&Value> {
    rel["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| {
            let n = a["name"].as_str().unwrap_or("");
            let low = n.to_lowercase();
            (low.ends_with(".zip") || low.ends_with(".7z")) && !NOT_WINDOWS.is_match(n)
        })
        .collect()
}

/// {"tag", "url", "name"} of the Windows archive in a repo's latest GitHub release.
pub fn github_latest(repo: &str) -> Result<Value, String> {
    let d = http_json(&format!("https://api.github.com/repos/{repo}/releases/latest"), None, &[], 30)?;
    let asset = windows_assets(&d).into_iter().next().ok_or(format!("{repo} has no Windows download in its latest release"))?;
    Ok(json!({"tag": d["tag_name"], "url": asset["browser_download_url"], "name": asset["name"]}))
}

fn ver(s: &str) -> Vec<u64> {
    static NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").unwrap());
    NUM.find_iter(s).map(|m| m.as_str().parse().unwrap_or(u64::MAX)).collect()
}

/// {"tag", "url"} of the exe in this app's newest manual-click release (its tag ends in "-manual"), or None when
/// this version is the newest. The other releases hold the automatic-click build, so an update never switches to it.
pub fn app_latest() -> Result<Option<Value>, String> {
    let all = http_json(&format!("https://api.github.com/repos/{APP_REPO}/releases"), None, &[], 30)?;
    let manual = |r: &&Value| r["tag_name"].as_str().is_some_and(|t| t.ends_with("-manual"));
    let Some(d) = all.as_array().into_iter().flatten().find(manual) else { return Ok(None) };
    let tag = d["tag_name"].as_str().ok_or("GitHub answered a release without a tag")?;
    if ver(tag) <= ver(shared::VERSION) {
        return Ok(None);
    }
    let own = format!("https://github.com/{APP_REPO}/releases/download/"); // only this repo's release files
    let asset = d["assets"].as_array().into_iter().flatten().find(|a| {
        a["name"].as_str().unwrap_or("").to_lowercase().ends_with(".exe")
            && a["browser_download_url"].as_str().unwrap_or("").starts_with(&own)
    });
    match asset {
        Some(a) => Ok(Some(json!({"tag": tag, "url": a["browser_download_url"]}))),
        None => Err(format!("Release {tag} has no exe to download")),
    }
}

/// A plain ASCII file name. Windows tools that read their command line in the ANSI code page turn some
/// Unicode letters into quotes ("best-fit"), so a server-chosen name must never reach them as it is.
pub fn ascii_name(name: &str) -> String {
    static BAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9._ ()+-]").unwrap());
    let s = BAD.replace_all(name, "_");
    let s = s.trim_matches(|c| c == '.' || c == ' ');
    if s.is_empty() { "download.zip".into() } else { s.into() }
}

/// The parts of a URL, split like Python's `urllib.parse.urlparse` (scheme lowercased, params dropped).
pub(crate) struct Url {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
}

impl Url {
    /// The host name: lowercased, without user info and port.
    pub fn hostname(&self) -> String {
        let h = self.netloc.rsplit('@').next().unwrap_or("");
        let h = if let Some(rest) = h.strip_prefix('[') { rest.split(']').next().unwrap_or("") } else { h.split(':').next().unwrap_or("") };
        h.to_lowercase()
    }
}

pub(crate) fn urlparse(s: &str) -> Url {
    let mut rest = s;
    let mut scheme = String::new();
    if let Some(i) = s.find(':')
        && i > 0
        && s.as_bytes()[0].is_ascii_alphabetic()
        && s[..i].bytes().all(|c| c.is_ascii_alphanumeric() || b"+-.".contains(&c))
    {
        scheme = s[..i].to_lowercase();
        rest = &s[i + 1..];
    }
    let mut netloc = "";
    if let Some(r) = rest.strip_prefix("//") {
        let end = r.find(['/', '?', '#']).unwrap_or(r.len());
        netloc = &r[..end];
        rest = &r[end..];
    }
    let rest = rest.split('#').next().unwrap_or("");
    let (mut path, query) = rest.split_once('?').unwrap_or((rest, ""));
    if ["", "http", "https"].contains(&scheme.as_str())
        && let Some(slash) = path.rfind('/')
        && let Some(semi) = path[slash..].find(';')
    {
        path = &path[..slash + semi];
    }
    Url { scheme, netloc: netloc.into(), path: path.into(), query: query.into() }
}

/// Percent-encode like Python's `urllib.parse.quote`: letters, digits, `_.-~` and the `safe` characters stay;
/// `plus` turns spaces into `+` (`quote_plus`).
pub(crate) fn quote(s: &str, safe: &str, plus: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) || safe.as_bytes().contains(&b) {
            out.push(b as char);
        } else if plus && b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decode `%xx` escapes (and `+` as a space when `plus`); bad UTF-8 becomes U+FFFD, like Python's `unquote`.
pub(crate) fn unquote(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c)))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(if plus && b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Python's `parse_qsl` into a dict: blank values are left out, a later key wins.
pub(crate) fn parse_qs(query: &str) -> HashMap<String, String> {
    let mut q = HashMap::new();
    for part in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        if !v.is_empty() {
            q.insert(unquote(k, true), unquote(v, true));
        }
    }
    q
}

/// SHA-1 (thumbnail cache names, the WebSocket handshake); the approved crates have no SHA-1.
pub(crate) fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend((data.len() as u64 * 8).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

/// {url: png path} of w x h crops of Nexus images, cached in folder. Images that fail are left out.
pub fn thumbnails(urls: &[String], folder: &Path, w: u32, h: u32) -> HashMap<String, PathBuf> {
    let _ = std::fs::create_dir_all(folder);
    let mut out: HashMap<String, PathBuf> = HashMap::new();
    for u in urls {
        let p = urlparse(u);
        if p.scheme == "https" && ["staticdelivery.nexusmods.com", "media.nexusmods.com"].contains(&p.hostname().as_str()) {
            let hex: String = sha1(u.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
            out.insert(u.clone(), folder.join(format!("{}_{w}x{h}.png", &hex[..16])));
        }
    }
    let todo: Vec<(&String, &PathBuf)> = out.iter().filter(|(_, p)| !p.exists()).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                while let Some((u, p)) = todo.get(next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)) {
                    let _ = thumbnail(u, p, w, h);
                }
            });
        }
    });
    out.retain(|_, p| p.exists());
    out
}

/// Scale one image to cover w x h, crop the middle and save it as PNG.
fn thumbnail(url: &str, dest: &Path, w: u32, h: u32) -> Result<(), String> {
    let mut req = Request::get(url).timeout(20);
    req.headers = merged(&UA, &[]);
    let mut s = open(&req)?;
    if !(200..300).contains(&s.status) {
        return Err(format!("HTTP Error {}", s.status));
    }
    let mut bytes = vec![];
    // thumbnails are small; a 5 MB cap keeps a bad file from filling memory
    s.body.by_ref().take(5 << 20).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    img.resize_to_fill(w, h, image::imageops::FilterType::Triangle)
        .save_with_format(dest, image::ImageFormat::Png)
        .map_err(|e| e.to_string())
}

/// Download url into folder. progress(done_bytes, total_bytes) is called about twice a second.
pub fn download(url: &str, folder: &Path, progress: Option<&dyn Fn(u64, u64)>, headers: &[(&str, &str)]) -> Result<PathBuf, String> {
    // Nexus CDN links have raw spaces; every ASCII punctuation mark (and so %xx) stays as it is
    let url = quote(url, "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~", false);
    let mut req = Request::get(&url).timeout(60);
    req.headers = merged(&UA, headers);
    let mut r = open(&req)?;
    if !(200..300).contains(&r.status) {
        return Err(format!("HTTP Error {}", r.status));
    }
    if urlparse(&r.url).scheme != "https" {
        // also catches an https link that redirects to http
        return Err("Refusing a download that is not https".into());
    }
    static CD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"filename\*?=(?:UTF-8'')?"?([^";]+)"#).unwrap());
    let cd = header_of(&r.headers, "Content-Disposition").unwrap_or("");
    let name = match CD.captures(cd) {
        Some(m) => m[1].to_string(),
        None => urlparse(&r.url).path.rsplit(['/', '\\']).next().unwrap_or("").to_string(),
    };
    let dest = folder.join(ascii_name(&unquote(&name, false)));
    let part = folder.join(format!("{}.part", dest.file_name().unwrap_or_default().to_string_lossy()));
    let total = header_of(&r.headers, "Content-Length").and_then(|v| v.trim().parse().ok()).unwrap_or(0u64);
    let (mut done, mut last) = (0u64, None::<Instant>);
    let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        let n = r.body.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        f.write_all(&chunk[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        if let Some(p) = progress
            && last.is_none_or(|t| t.elapsed() > Duration::from_millis(500))
        {
            p(done, total);
            last = Some(Instant::now());
        }
    }
    drop(f);
    std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
    Ok(dest)
}
