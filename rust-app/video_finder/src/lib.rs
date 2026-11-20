//! Find the Cyberpunk 2077 mods a video shows, from the text around it: mod links in the description first,
//! else a Nexus name search with the title, the chapter titles or the caption. YouTube gives the most text
//! (full description with links and chapters); TikTok and Instagram give the caption; other sites give the
//! page title and summary. The video itself is never watched.
use downloads::fetch::{self, Request};
use downloads::nexus;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::sync::{Arc, LazyLock, RwLock};

#[cfg(test)]
mod tests;

// SOCS=CAI: "reject all" on Google's cookie consent page, so YouTube in the EU shows the video page itself.
pub const HEADERS: [(&str, &str); 3] = [
    ("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"),
    ("Accept-Language", "en-US,en;q=0.9"),
    ("Cookie", "SOCS=CAI"),
];
pub static LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:https?://)?(?:[\w-]+\.)*(?:nexusmods\.com|github\.com)/[^\s<>"'()\[\]]+"#).unwrap()
});
pub static CHAPTER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\W*(?:\d{1,2}:)?\d{1,2}:\d{2}\W*(.+)$").unwrap());
pub static NOISE: LazyLock<HashSet<&str>> = LazyLock::new(|| {
    "cyberpunk cyberpunk2077 2077 cp2077 cp77 mod mods modded modding showcase nexus nexusmods the a an this that these
my your you i is are was in on at for of to with and or by how install installing installed guide tutorial best top new
most pc 4k 8k rtx gameplay game now just it its makes make look looks like must have need needs only update patch version
fyp foryou foryoupage shorts short video intro outro part ultra"
        .split_whitespace()
        .collect()
});

/// The most of a page that is read.
const LIMIT: usize = 8_000_000;

/// Name -> addresses, in place of the system's DNS lookup (tests).
pub type Resolver = Arc<dyn Fn(&str) -> Result<Vec<IpAddr>, String> + Send + Sync>;

static RESOLVER: RwLock<Option<Resolver>> = RwLock::new(None);

/// Tests: answer every name lookup in `public` with `r`, or `None` for the system's DNS.
pub fn set_resolver(r: Option<Resolver>) {
    *RESOLVER.write().unwrap_or_else(|e| e.into_inner()) = r;
}

fn resolve(host: &str) -> Result<Vec<IpAddr>, String> {
    let hook = RESOLVER.read().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(r) = hook {
        return r(host);
    }
    Ok((host, 0).to_socket_addrs().map_err(|e| e.to_string())?.map(|a| a.ip()).collect())
}

fn inside(x: u128, nets: &[(u128, u32)], width: u32) -> bool {
    nets.iter().any(|&(n, bits)| x >> (width - bits) == n >> (width - bits))
}

/// Python's `IPv4Address.is_private`.
fn v4_private(a: Ipv4Addr) -> bool {
    let n = |a, b, c, d, bits| (Ipv4Addr::new(a, b, c, d).to_bits() as u128, bits);
    let nets = [
        n(0, 0, 0, 0, 8),
        n(10, 0, 0, 0, 8),
        n(127, 0, 0, 0, 8),
        n(169, 254, 0, 0, 16),
        n(172, 16, 0, 0, 12),
        n(192, 0, 0, 0, 24),
        n(192, 0, 0, 170, 31),
        n(192, 0, 2, 0, 24),
        n(192, 168, 0, 0, 16),
        n(198, 18, 0, 0, 15),
        n(198, 51, 100, 0, 24),
        n(203, 0, 113, 0, 24),
        n(240, 0, 0, 0, 4),
        n(255, 255, 255, 255, 32),
    ];
    let except = [n(192, 0, 0, 9, 32), n(192, 0, 0, 10, 32)];
    let x = a.to_bits() as u128;
    inside(x, &nets, 32) && !inside(x, &except, 32)
}

/// Python's `IPv6Address.is_private` for an address that is not IPv4-mapped.
fn v6_private(a: Ipv6Addr) -> bool {
    let n = |s: [u16; 3], bits| (Ipv6Addr::new(s[0], s[1], s[2], 0, 0, 0, 0, 0).to_bits(), bits);
    let nets = [
        ("::1".parse::<Ipv6Addr>().unwrap().to_bits(), 128),
        (0, 128),
        ("::ffff:0:0".parse::<Ipv6Addr>().unwrap().to_bits(), 96),
        n([0x64, 0xff9b, 1], 48),
        n([0x100, 0, 0], 64),
        n([0x2001, 0, 0], 23),
        n([0x2001, 0xdb8, 0], 32),
        n([0x2002, 0, 0], 16),
        n([0x3fff, 0, 0], 20),
        n([0xfc00, 0, 0], 7),
        n([0xfe80, 0, 0], 10),
    ];
    let except = [
        ("2001:1::1".parse::<Ipv6Addr>().unwrap().to_bits(), 128),
        ("2001:1::2".parse::<Ipv6Addr>().unwrap().to_bits(), 128),
        n([0x2001, 3, 0], 32),
        n([0x2001, 4, 0x112], 48),
        n([0x2001, 0x20, 0], 28),
        n([0x2001, 0x30, 0], 28),
    ];
    let x = a.to_bits();
    inside(x, &nets, 128) && !inside(x, &except, 128)
}

/// Python's `ip_address(...).is_global`, with an IPv4-mapped IPv6 address checked as the IPv4 address.
fn is_global(ip: IpAddr) -> bool {
    let v4 = |a: Ipv4Addr| !inside(a.to_bits() as u128, &[(Ipv4Addr::new(100, 64, 0, 0).to_bits() as u128, 10)], 32) && !v4_private(a);
    match ip {
        IpAddr::V4(a) => v4(a),
        IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(m) => v4(m),
            None => !v6_private(a),
        },
    }
}

/// Refuse a link to this PC or the local network (a router page, another app's local server): the link comes
/// from the user, an AI app or a redirect, and only a public web page belongs here.
pub fn public(url: &str) -> Result<(), String> {
    let bad = || "Paste an http:// or https:// link".to_string();
    // the HTTP client's own parser, so the checked host is the host it connects to
    let uri: ureq::http::Uri = url.parse().map_err(|_| bad())?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return Err(bad());
    }
    let host = uri.host().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_lowercase();
    if host.is_empty() {
        return Err(bad());
    }
    for ip in resolve(&host)? {
        if !is_global(ip) {
            return Err(format!("{host} is on this PC or your local network: paste a link to a public video page"));
        }
    }
    Ok(())
}

/// A redirect target as an absolute URL (Python's `urljoin`), with spaces and non-ASCII bytes %-encoded.
fn redirect_target(base: &str, location: &str) -> String {
    let loc: String = location
        .bytes()
        .map(|b| if (0x21..=0x7e).contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect();
    let b = urlparse(base);
    if !urlparse(&loc).scheme.is_empty() {
        return loc;
    }
    if loc.starts_with("//") {
        return format!("{}:{loc}", b.scheme);
    }
    let origin = format!("{}://{}", b.scheme, b.netloc);
    if loc.is_empty() {
        base.to_string()
    } else if loc.starts_with('/') {
        origin + &loc
    } else if loc.starts_with('?') {
        format!("{origin}{}{loc}", b.path)
    } else {
        let dir = &b.path[..b.path.rfind('/').map_or(0, |i| i + 1)];
        format!("{origin}{}{loc}", if dir.is_empty() { "/" } else { dir })
    }
}

/// The page at url as text, and the address it ended at after redirects.
pub fn page(url: &str, limit: usize) -> Result<(String, String), String> {
    // ponytail: the name is looked up again to connect, so a DNS server that answers differently the second time
    // (rebinding) gets through; connect to the checked address if that ever matters
    public(url)?;
    let mut url = url.to_string();
    for _ in 0..=10 {
        let mut req = Request::get(&url).timeout(30);
        req.redirects = 0; // every redirect is checked with public() before it is followed
        for (k, v) in HEADERS {
            req = req.header(k, v);
        }
        let r = fetch::send(&req)?;
        if [301, 302, 303, 307, 308].contains(&r.status)
            && let Some(loc) = r.header("Location")
        {
            let next = redirect_target(&url, loc);
            public(&next)?;
            url = next;
            continue;
        }
        if !(200..300).contains(&r.status) {
            return Err(format!("HTTP Error {}", r.status));
        }
        let body = &r.body[..r.body.len().min(limit)];
        return Ok((String::from_utf8_lossy(body).into_owned(), url));
    }
    Err("HTTP Error 302: the page redirects too often".into())
}

/// The parts of a URL, split like Python's `urllib.parse.urlparse`.
struct Url {
    scheme: String,
    netloc: String,
    path: String,
    query: String,
}

fn urlparse(s: &str) -> Url {
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
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    Url { scheme, netloc: netloc.into(), path: path.into(), query: query.into() }
}

/// Decode `%xx` escapes and `+` as a space (Python's `unquote_plus`).
fn unquote_plus(s: &str) -> String {
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
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn youtube_id(url: &str) -> Option<String> {
    static PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/(?:shorts|live|embed)/([\w-]{11})").unwrap());
    let u = urlparse(url);
    let low = u.netloc.to_lowercase();
    let host = low.strip_prefix("www.").unwrap_or(&low);
    let host = host.strip_prefix("m.").unwrap_or(host);
    if host == "youtu.be" {
        let id = first_chars(u.path.trim_matches('/'), 11);
        return (!id.is_empty()).then_some(id);
    }
    if host.ends_with("youtube.com") {
        let v = u.query.split('&').filter_map(|p| p.split_once('=')).find(|(k, v)| unquote_plus(k) == "v" && !v.is_empty());
        if let Some((_, v)) = v {
            return Some(first_chars(&unquote_plus(v), 11));
        }
        return PATH.captures(&u.path).map(|c| c[1].to_string());
    }
    None
}

/// The JSON value that starts after `marker` in text, or None.
fn json_after(text: &str, marker: &str) -> Option<Value> {
    let i = text.find(marker)?;
    let j = i + text[i..].find('{')?;
    serde_json::Deserializer::from_str(&text[j..]).into_iter::<Value>().next()?.ok()
}

/// Python's `html.unescape` for numeric references and the common named ones.
fn unescape(s: &str) -> String {
    static REF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&(?:#[0-9]+;?|#[xX][0-9a-fA-F]+;?|[A-Za-z][A-Za-z0-9]*;)").unwrap());
    REF.replace_all(s, |c: &regex::Captures| {
        let r = c[0].trim_start_matches('&').trim_end_matches(';');
        let num = if let Some(h) = r.strip_prefix("#x").or_else(|| r.strip_prefix("#X")) {
            Some(u32::from_str_radix(h, 16).unwrap_or(u32::MAX))
        } else {
            r.strip_prefix('#').map(|d| d.parse().unwrap_or(u32::MAX))
        };
        if let Some(n) = num {
            return match n {
                0 => '\u{fffd}'.to_string(),
                n => char::from_u32(n).unwrap_or('\u{fffd}').to_string(),
            };
        }
        let named = match r {
            "amp" => "&",
            "lt" => "<",
            "gt" => ">",
            "quot" => "\"",
            "apos" => "'",
            "nbsp" => "\u{a0}",
            "hellip" => "\u{2026}",
            "mdash" => "\u{2014}",
            "ndash" => "\u{2013}",
            "lsquo" => "\u{2018}",
            "rsquo" => "\u{2019}",
            "ldquo" => "\u{201c}",
            "rdquo" => "\u{201d}",
            "copy" => "\u{a9}",
            "reg" => "\u{ae}",
            "trade" => "\u{2122}",
            _ => return c[0].to_string(),
        };
        named.to_string()
    })
    .into_owned()
}

/// {name: content} of a page's <meta> tags (og:title, description, ...).
fn meta(text: &str) -> HashMap<String, String> {
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<meta\b[^<>]*>").unwrap());
    // Python's pattern has (?<![\w:-]) in front; a left-to-right scan never starts inside a name, so it is not needed
    static ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([\w:-]+)\s*=\s*"([^"]*)""#).unwrap());
    let mut out = HashMap::new();
    for tag in TAG.find_iter(text) {
        let mut a: HashMap<String, String> = HashMap::new();
        for c in ATTR.captures_iter(tag.as_str()) {
            a.insert(c[1].to_lowercase(), unescape(&c[2]));
        }
        let key = a.get("property").filter(|p| !p.is_empty()).or(a.get("name")).cloned().unwrap_or_default();
        out.entry(key).or_insert_with(|| a.get("content").cloned().unwrap_or_default());
    }
    out
}

fn text_of(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// (title, description or caption) of the video at url.
pub fn video_text(url: &str) -> Result<(String, String), String> {
    static VIDEO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/video/(\d+)").unwrap());
    static POST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/(?:p|reels?|tv)/([\w-]+)").unwrap());
    static USER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)<a class="CaptionUsername".*?</a>"#).unwrap());
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^<>]*>").unwrap());
    static BR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<br\s*/?>").unwrap());
    static TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<title[^<>]*>([^<]*)</title>").unwrap());
    let u = urlparse(url);
    let host = u.netloc.to_lowercase();
    if let Some(vid) = youtube_id(url) {
        let d = json_after(&page(&format!("https://www.youtube.com/watch?v={vid}"), LIMIT)?.0, "ytInitialPlayerResponse");
        let v = d.map(|d| d["videoDetails"].clone()).unwrap_or(Value::Null);
        if !v.as_object().is_some_and(|o| !o.is_empty()) {
            return Err("YouTube did not show this video (private, removed or age-restricted?)".into());
        }
        return Ok((text_of(&v["title"]), text_of(&v["shortDescription"])));
    }
    if host.ends_with("tiktok.com") {
        let id = match VIDEO.captures(url) {
            Some(c) => Some(c[1].to_string()),
            None => VIDEO.captures(&page(url, 200_000)?.1).map(|c| c[1].to_string()), // short links redirect
        };
        let id = id.ok_or("This TikTok link has no video number in it")?;
        let d = json_after(&page(&format!("https://www.tiktok.com/embed/{id}"), LIMIT)?.0, r#"id="__FRONTITY_CONNECT_STATE__""#)
            .unwrap_or(Value::Null);
        let info = &d["source"]["data"][format!("/embed/{id}").as_str()]["videoData"]["itemInfos"];
        return Ok((String::new(), text_of(&info["text"])));
    }
    if host.ends_with("instagram.com")
        && let Some(m) = POST.captures(&u.path)
    {
        let text = page(&format!("https://www.instagram.com/p/{}/embed/captioned/", &m[1]), LIMIT)?.0;
        // str::find, not a pattern: the page can be 8 MB
        let i = text.find(r#"<div class="Caption""#);
        let j = i.and_then(|i| text[i..].find(r#"<div class="CaptionComments""#).map(|j| i + j));
        let cap = match (i, j) {
            (Some(i), Some(j)) if i < j => {
                let start = text[i..].find('>').map_or(0, |g| i + g + 1).min(j);
                first_chars(&text[start..j], 20_000) // a caption has at most 2,200 letters
            }
            _ => String::new(),
        };
        let cap = USER.replace_all(&cap, "");
        // Python: <(?!br)[^<>]*> (the regex crate has no look-ahead), so keep the tags that start with "<br"
        let cap = TAG.replace_all(&cap, |c: &regex::Captures| if c[0].starts_with("<br") { c[0].to_string() } else { String::new() });
        return Ok((String::new(), unescape(&BR.replace_all(&cap, "\n")).trim().to_string()));
    }
    let text = page(url, LIMIT)?.0;
    let meta = meta(&text);
    let pick = |k: &str| meta.get(k).filter(|v| !v.is_empty()).cloned();
    let title = pick("og:title")
        .or_else(|| pick("twitter:title"))
        .unwrap_or_else(|| TITLE.captures(&text).map(|t| unescape(&t[1])).unwrap_or_default());
    let desc = pick("og:description").or_else(|| pick("description")).unwrap_or_default();
    Ok((title.trim().to_string(), desc.trim().to_string()))
}

/// Mods linked in the text: Nexus mods and collections for Cyberpunk, and GitHub repositories.
pub fn links(text: &str) -> Result<Vec<Value>, String> {
    let (mut seen, mut rows, mut ids) = (HashSet::new(), vec![], vec![]);
    let mut row_ids: Vec<Option<i64>> = vec![];
    for raw in LINK.find_iter(text) {
        let url = raw.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?']);
        let full = if url.to_lowercase().starts_with("http") { url.to_string() } else { format!("https://{url}") };
        // another game, a user page, the game's front page...
        let Ok(Some(link)) = nexus::parse_link(&full) else { continue };
        let kind = link["type"].as_str().unwrap_or("").to_string();
        let id = [&link["mod_id"], &link["slug"], &link["repo"]].into_iter().find(|v| !v.is_null()).cloned().unwrap_or(Value::Null);
        if !["nexus", "collection", "github"].contains(&kind.as_str()) || !seen.insert((kind.clone(), id.to_string())) {
            continue;
        }
        if kind == "nexus" {
            let mod_id = link["mod_id"].as_i64().unwrap_or(0);
            ids.push(mod_id);
            row_ids.push(Some(mod_id));
            rows.push(json!({"name": format!("Nexus mod {mod_id}"), "url": format!("https://www.nexusmods.com/{}/mods/{mod_id}", nexus::GAME)}));
        } else if kind == "collection" {
            let slug = text_of(&link["slug"]);
            // removed or hidden: keep the link, the user can open it
            let c = nexus::collection_info(&slug).unwrap_or(Value::Null);
            let name = c.get("name").and_then(Value::as_str).map(String::from).unwrap_or(format!("Collection {slug}"));
            row_ids.push(None);
            rows.push(json!({"name": name, "endorsements": c.get("endorsements").cloned().unwrap_or(Value::Null),
                             "url": format!("https://www.nexusmods.com/games/{}/collections/{slug}", nexus::GAME)}));
        } else {
            let repo = text_of(&link["repo"]);
            row_ids.push(None);
            rows.push(json!({"name": repo, "url": format!("https://github.com/{repo}")}));
        }
    }
    let names = if ids.is_empty() { Default::default() } else { nexus::mod_names(&ids)? };
    for (r, id) in rows.iter_mut().zip(row_ids) {
        let n = id.and_then(|i| names.get(&i)).cloned().unwrap_or(json!({}));
        if let Some(name) = n.get("name") {
            r["name"] = name.clone();
        }
        r["endorsements"] = n.get("endorsements").or(r.get("endorsements")).cloned().unwrap_or(Value::Null);
        r["why"] = json!("Linked in the video");
    }
    Ok(rows)
}

/// Python's `str.splitlines`.
fn splitlines(s: &str) -> Vec<&str> {
    let (mut out, mut start) = (vec![], 0);
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            out.push(&s[start..i]);
            start = i + c.len_utf8();
            if c == '\r' && it.peek().is_some_and(|&(_, n)| n == '\n') {
                it.next();
                start += 1;
            }
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Word lists worth a Nexus name search: the title, then the chapter titles or else the first lines of the text.
/// Each is cut at separators (| - : ! ( ...), and common words (cyberpunk, mod, showcase, the ...) are dropped.
pub fn phrases(title: &str, text: &str) -> Vec<Vec<String>> {
    static LINKS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"https?://\S+|[#@][\w.]+").unwrap());
    static SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\s[-–—]\s|[|•:;!?()\[\]{}"“”/,+&]"#).unwrap());
    static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\w']+").unwrap());
    let chapters: Vec<&str> = CHAPTER.captures_iter(text).map(|c| c.get(1).map_or("", |m| m.as_str())).collect();
    let rest = if chapters.is_empty() { splitlines(text).into_iter().take(5).collect() } else { chapters };
    let mut out: Vec<Vec<String>> = vec![];
    for piece in std::iter::once(title).chain(rest) {
        let piece = LINKS.replace_all(piece, " ");
        for seg in SEP.split(&piece) {
            let words: Vec<String> = WORD
                .find_iter(seg)
                .map(|w| w.as_str().trim_matches('\'').to_string())
                .filter(|w| !w.is_empty() && !NOISE.contains(w.to_lowercase().as_str()) && !w.chars().all(char::is_numeric))
                .take(6)
                .collect();
            if words.iter().any(|w| w.chars().count() >= 3) && !out.contains(&words) {
                out.push(words);
            }
        }
    }
    out.truncate(6);
    out
}

/// (video title, [{"name", "url", "why", "endorsements"}]) for a video link, or for pasted text such as a
/// caption or a mod name. Links in the video win; the name search runs only when it links no Nexus mod.
pub fn find(entry: &str) -> Result<(String, Vec<Value>), String> {
    static BARE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(?:www\.|m\.)?[\w-]+(?:\.[\w-]+)*\.[a-z]{2,}/\S*$").unwrap());
    static WEB: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://\S+$").unwrap());
    let mut entry = entry.trim().to_string();
    // youtu.be/x without https:// (a Nexus or GitHub link is read as it is)
    if entry.chars().count() < 2000 && BARE.is_match(&entry) && !LINK.find(&entry).is_some_and(|m| m.start() == 0) {
        entry = format!("https://{entry}");
    }
    let (title, text) = if WEB.is_match(&entry) { video_text(&entry)? } else { (String::new(), entry) };
    // YouTube's limit; much more is a page that makes the patterns below slow
    let (title, text) = (first_chars(&title, 300), first_chars(&text, 5000));
    let mut rows = links(&format!("{title}\n{text}"))?;
    if rows.iter().any(|r| text_of(&r["url"]).contains("nexusmods")) {
        return Ok((title, rows));
    }
    let mut seen = HashSet::new();
    for words in phrases(&title, &text) {
        let w: Vec<&str> = words.iter().map(String::as_str).collect();
        for n in nexus::search(&w)?.into_iter().take(3) {
            if seen.insert(n["modId"].to_string()) {
                let id = n["modId"].as_i64().map_or_else(|| text_of(&n["modId"]), |i| i.to_string());
                rows.push(json!({"name": n["name"], "url": format!("https://www.nexusmods.com/{}/mods/{id}", nexus::GAME),
                                 "why": format!("Name matches \"{}\"", words.join(" ")), "endorsements": n["endorsements"]}));
            }
        }
    }
    Ok((title, rows))
}
