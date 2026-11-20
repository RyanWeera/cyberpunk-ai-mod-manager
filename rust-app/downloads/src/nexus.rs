//! Nexus Mods: link parsing, mod and collection lookups (GraphQL, no key needed) and download links (v1 REST).
//!
//! Free accounts: Nexus gives a download link only for a file the user clicked "Slow download" on in the
//! browser. That click sends an nxm:// link (with key + expires) to this app, and the rest is automatic.
//! Premium accounts get links straight from the API, so nothing has to be clicked.
use crate::fetch::{http_json, parse_qs, quote, urlparse, windows_assets};
use regex::Regex;
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

pub const GAME: &str = "cyberpunk2077";
pub const GAME_ID: i64 = 3333;
pub const GQL: &str = "https://api.nexusmods.com/v2/graphql";
pub const API: &str = "https://api.nexusmods.com/v1";
pub const GONE: [&str; 4] = ["ARCHIVED", "REMOVED", "DELETED", "OLD_VERSION"];
/// Game update 2.0 changed scripts and game files: many mods not updated since then stopped working.
pub const GAME_2_0: &str = "2023-09-21";
pub static OUTDATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)outdated|broken|deprecated|obsolete|discontinued|abandoned|incompatible|not (?:compatible|working|updated)|no longer (?:works?|supported|updated|maintained)",
    )
    .unwrap()
});

/// Why a mod may not work with the game any more, or "" when nothing says so. An empty status means "published".
pub fn outdated(name: &str, status: &str, updated: &str) -> String {
    static FIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)fix|patch").unwrap());
    if !status.is_empty() && status != "published" {
        return "the author removed or hid it on Nexus".into();
    }
    // "Fix for broken X" is a fix, not a broken mod
    if OUTDATED.is_match(name) && !FIX.is_match(name) {
        return "its name says it is outdated or broken".into();
    }
    if !updated.is_empty() && updated < GAME_2_0 {
        let day: String = updated.chars().take(10).collect();
        return format!("it was last updated on {day}, before game update 2.0 changed how mods work");
    }
    String::new()
}

/// Text of a JSON value: strings as they are, other values as JSON, null as "".
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// A Nexus id that may come as a number or a string.
fn int(v: &Value) -> Result<i64, String> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).ok_or_else(|| format!("Nexus answered a bad id: {v}"))
}

fn need<'a>(v: &'a Value, key: &str) -> Result<&'a Value, String> {
    match v.get(key) {
        Some(x) if !x.is_null() => Ok(x),
        _ => Err(format!("Nexus answered without '{key}'")),
    }
}

fn path<'a>(v: &'a Value, keys: &[&str]) -> Result<&'a Value, String> {
    keys.iter().try_fold(v, |v, k| need(v, k))
}

fn list(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// Python truthiness.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `v or default`.
fn or(v: Option<&Value>, default: Value) -> Value {
    v.filter(|v| truthy(v)).cloned().unwrap_or(default)
}

/// Python `v.get(key, default)`: a key that holds null stays null.
fn get(v: &Value, key: &str, default: Value) -> Value {
    v.get(key).cloned().unwrap_or(default)
}

/// Python ordering of numbers (bools count as 0/1) and strings.
fn cmp_val(a: &Value, b: &Value) -> Ordering {
    let num = |v: &Value| v.as_f64().or_else(|| v.as_bool().map(|b| b as u8 as f64));
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => text(a).cmp(&text(b)),
    }
}

/// The first item with the largest key (Python's `max`).
fn max_by<'a>(items: impl IntoIterator<Item = &'a Value>, cmp: impl Fn(&Value, &Value) -> Ordering) -> Option<&'a Value> {
    items.into_iter().fold(None, |best, f| match best {
        Some(b) if cmp(f, b) != Ordering::Greater => Some(b),
        _ => Some(f),
    })
}

/// One GraphQL call; the `data` of the answer, or the first error's message.
pub fn gql(query: &str, api_key: &str, variables: Value) -> Result<Value, String> {
    let headers: &[(&str, &str)] = if api_key.is_empty() { &[] } else { &[("apikey", api_key)] };
    let d = http_json(GQL, Some(&json!({"query": query, "variables": variables})), headers, 30)?;
    if let Some(e) = d.get("errors").filter(|e| truthy(e)) {
        return Err(e[0].get("message").and_then(Value::as_str).unwrap_or("Nexus API error").to_string());
    }
    Ok(d.get("data").cloned().unwrap_or(Value::Null))
}

pub fn mod_info(mod_id: i64, api_key: &str) -> Result<Value, String> {
    let d = gql(
        r#"query($id: ID!, $game: ID!) {
      mod(modId: $id, gameId: $game) { name version status updatedAt
        modRequirements { nexusRequirements { nodes { modId modName gameId externalRequirement } } } }
      modFiles(modId: $id, gameId: $game) { fileId name version category primary date } }"#,
        api_key,
        json!({"id": mod_id.to_string(), "game": GAME_ID.to_string()}),
    )?;
    let m = need(&d, "mod")?;
    let mut requires = vec![];
    for r in list(path(m, &["modRequirements", "nexusRequirements", "nodes"])?) {
        if truthy(&r["externalRequirement"]) || text(&r["gameId"]) != GAME_ID.to_string() {
            continue;
        }
        let id = int(&r["modId"])?;
        if id != mod_id {
            requires.push(json!({"mod_id": id, "name": r["modName"]}));
        }
    }
    let name = text(&m["name"]);
    Ok(json!({"name": m["name"], "version": m["version"], "files": d["modFiles"],
              "outdated": outdated(&name, &text(&m["status"]), &text(&m["updatedAt"])), "requires": requires}))
}

/// The asked-for file, else the newest primary MAIN file, else the newest file still listed.
pub fn pick_file(files: &[Value], file_id: Option<i64>) -> Option<&Value> {
    if let Some(id) = file_id.filter(|&i| i != 0) {
        return files.iter().find(|f| int(&f["fileId"]).ok() == Some(id));
    }
    let main: Vec<&Value> = files.iter().filter(|f| f["category"] == "MAIN").collect();
    let pool = if main.is_empty() {
        files.iter().filter(|f| !GONE.contains(&f["category"].as_str().unwrap_or(""))).collect()
    } else {
        main
    };
    max_by(pool, |a, b| cmp_val(&a["primary"], &b["primary"]).then_with(|| cmp_val(&a["date"], &b["date"])))
}

/// A newer upload that replaces file_id (same file name first), or None when file_id is current.
pub fn newer_file(files: &[Value], file_id: i64) -> Option<&Value> {
    let cur = files.iter().find(|f| int(&f["fileId"]).ok() == Some(file_id));
    let same = files.iter().filter(|f| {
        cur.is_some_and(|c| f["name"] == c["name"]) && !GONE.contains(&f["category"].as_str().unwrap_or(""))
    });
    let best = max_by(same, |a, b| cmp_val(&a["date"], &b["date"])).or_else(|| pick_file(files, None))?;
    let later = cur.is_none_or(|c| cmp_val(&best["date"], &c["date"]) == Ordering::Greater);
    (int(&best["fileId"]).ok() != Some(file_id) && later).then_some(best)
}

pub fn collection(slug: &str, revision: Option<i64>, api_key: &str) -> Result<Value, String> {
    let d = gql(
        r#"query($slug: String!, $rev: Int) {
      collectionRevision(slug: $slug, revision: $rev, domainName: "cyberpunk2077", viewAdultContent: true) {
        revisionNumber collection { name }
        modFiles { fileId optional file { name version mod { modId name status updatedAt } } } } }"#,
        api_key,
        json!({"slug": slug, "rev": revision}),
    )?;
    let rev = need(&d, "collectionRevision")?;
    let mut files = vec![];
    for f in list(&rev["modFiles"]).iter().filter(|f| truthy(&f["file"])) {
        let m = need(&f["file"], "mod")?;
        files.push(json!({"mod_id": int(&m["modId"])?, "file_id": int(&f["fileId"])?, "optional": f["optional"],
                          "name": m["name"], "version": f["file"]["version"],
                          "outdated": outdated(&text(&m["name"]), &text(&m["status"]), &text(&m["updatedAt"]))}));
    }
    Ok(json!({"name": path(rev, &["collection"])?["name"], "revision": rev["revisionNumber"], "files": files}))
}

/// {mod id: {"modId", "name", "endorsements"}} for Cyberpunk mods, in one request. Hidden or deleted mods are left out.
pub fn mod_names(ids: &[i64]) -> Result<BTreeMap<i64, Value>, String> {
    let d = gql(
        r#"query($ids: [CompositeDomainWithIdInput!]!, $n: Int) {
      legacyModsByDomain(ids: $ids, count: $n) { nodes { modId name endorsements } } }"#,
        "",
        json!({"ids": ids.iter().map(|i| json!({"gameDomain": GAME, "modId": i})).collect::<Vec<_>>(), "n": ids.len()}),
    )?;
    let mut out = BTreeMap::new();
    for n in list(path(&d, &["legacyModsByDomain", "nodes"])?) {
        out.insert(int(&n["modId"])?, n.clone());
    }
    Ok(out)
}

/// [{"mod_id", "name"}] of the Cyberpunk mods that list mod_id as a requirement (patches and add-ons for it).
/// Nexus gives at most 80 per request, so the pages are asked for at the same time; `most` caps very popular mods.
pub fn mods_requiring(mod_id: i64, most: i64) -> Result<Vec<Value>, String> {
    let q = r#"query($id: ID!, $game: ID!, $o: Int) { mod(modId: $id, gameId: $game) { modRequirements {
      modsRequiringThisMod(offset: $o, count: 80) { totalCount nodes { modId modName gameId } } } } }"#;
    let page = |o: i64| -> Result<Value, String> {
        let d = gql(q, "", json!({"id": mod_id.to_string(), "game": GAME_ID.to_string(), "o": o}))?;
        path(&d, &["mod", "modRequirements", "modsRequiringThisMod"]).cloned()
    };
    let first = page(0)?;
    let mut nodes: Vec<Value> = list(&first["nodes"]).to_vec();
    let total = first["totalCount"].as_i64().unwrap_or(0);
    let offsets: Vec<i64> = (80..total.min(most)).step_by(80).collect();
    for batch in offsets.chunks(6) {
        let pages: Vec<Result<Value, String>> = std::thread::scope(|s| {
            let jobs: Vec<_> = batch.iter().map(|&o| s.spawn(move || page(o))).collect();
            jobs.into_iter().map(|j| j.join().unwrap_or_else(|_| Err("a Nexus page lookup failed".into()))).collect()
        });
        for p in pages {
            nodes.extend(list(&p?["nodes"]).iter().cloned());
        }
    }
    let mut out = vec![];
    for n in nodes.iter().filter(|n| text(&n["gameId"]) == GAME_ID.to_string()) {
        out.push(json!({"mod_id": int(&n["modId"])?, "name": n["modName"]}));
    }
    Ok(out)
}

/// {"repo", "url"} of the author's own GitHub release of a Nexus mod, so a free account needs no click; else None.
/// The repo must be linked on the mod's Nexus page, carry a word of the mod's name and not be a framework. Its latest
/// release must have the same version as the Nexus file and exactly one Windows archive (no guess between variants).
pub fn github_copy(mod_id: i64, version: &str) -> Result<Option<Value>, String> {
    static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9]{4,}").unwrap());
    static REPO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"github\.com/(\w[\w.-]*/\w[\w.-]*)").unwrap());
    static NOT_ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]").unwrap());
    let norm = |v: &str| v.to_lowercase().trim_start_matches('v').trim().to_string();
    let want = norm(version);
    if want.is_empty() {
        return Ok(None);
    }
    let d = gql(
        "query($id: ID!, $game: ID!) { mod(modId: $id, gameId: $game) { name description } }",
        "",
        json!({"id": mod_id.to_string(), "game": GAME_ID.to_string()}),
    )?;
    let m = need(&d, "mod")?;
    let name = text(&m["name"]).to_lowercase();
    let words: Vec<&str> = WORD.find_iter(&name).map(|w| w.as_str()).filter(|w| !["cyberpunk", "2077", "mods"].contains(w)).collect();
    let desc = text(&m["description"]);
    let mut seen = HashSet::new();
    for c in REPO.captures_iter(&desc) {
        let found = c.get(1).map_or("", |x| x.as_str());
        if !seen.insert(found) {
            continue;
        }
        let repo = found.strip_suffix(".git").unwrap_or(found);
        let short = NOT_ALNUM.replace_all(&repo.split('/').nth(1).unwrap_or("").to_lowercase(), "").into_owned();
        if crate::fetch::fw_by_repo(repo).is_some() || !words.iter().any(|w| short.contains(w)) {
            continue;
        }
        // no releases, or GitHub's hourly limit: the Nexus click still works
        let Ok(rel) = http_json(&format!("https://api.github.com/repos/{repo}/releases/latest"), None, &[], 30) else {
            continue;
        };
        let assets = windows_assets(&rel);
        if norm(rel["tag_name"].as_str().unwrap_or("")) == want
            && assets.len() == 1
            && assets[0]["browser_download_url"].as_str().unwrap_or("").starts_with("https://github.com/")
        {
            return Ok(Some(json!({"repo": repo, "url": assets[0]["browser_download_url"]})));
        }
    }
    Ok(None)
}

/// {"name", "endorsements"} of a Cyberpunk collection, without its file list. An error when it is gone.
pub fn collection_info(slug: &str) -> Result<Value, String> {
    let q = format!(
        r#"query($s: String!) {{ collection(slug: $s, domainName: "{GAME}", viewAdultContent: true) {{
      name endorsements }} }}"#
    );
    Ok(gql(&q, "", json!({"s": slug}))?["collection"].clone())
}

/// Cyberpunk mods with any of `words` in their name, best first. Mods with every word come first, the most
/// endorsed of them first (the original before its translations); then partial matches in Nexus relevance order.
pub fn search(words: &[&str]) -> Result<Vec<Value>, String> {
    let q = format!(
        r#"query($w: [BaseFilterValue!], $sort: [ModsSort!]) {{ mods(filter: {{op: AND,
      gameDomainName: [{{value: "{GAME}", op: EQUALS}}], filter: [{{op: OR, nameStemmed: $w}}]}}, sort: $sort, count: 20) {{
      nodes {{ modId name endorsements }} }} }}"#
    );
    let w: Vec<Value> = words.iter().map(|x| json!({"value": x, "op": "MATCHES"})).collect();
    let (mut found, mut seen) = (vec![], HashSet::new());
    for sort in ["relevance", "endorsements"] {
        let d = gql(&q, "", json!({"w": w, "sort": [{sort: {"direction": "DESC"}}]}))?;
        for (rank, n) in list(path(&d, &["mods", "nodes"])?).iter().enumerate() {
            if seen.insert(text(&n["modId"])) {
                let mut n = n.clone();
                n["rank"] = json!(if sort == "relevance" { rank } else { 99 });
                found.push(n);
            }
        }
    }
    let key = |n: &Value| {
        let name = text(&n["name"]).to_lowercase();
        let hits = words.iter().filter(|x| name.contains(&x.to_lowercase())).count() as i64;
        if hits == words.len() as i64 {
            (-hits, 0, -n["endorsements"].as_i64().unwrap_or(0))
        } else {
            (-hits, 1, n["rank"].as_i64().unwrap_or(0))
        }
    };
    found.sort_by_key(key);
    Ok(found)
}

pub const SORTS: [(&str, &str); 5] = [
    ("Most downloaded", "downloads"),
    ("Most endorsed", "endorsements"),
    ("Recently updated", "updatedAt"),
    ("Newest", "createdAt"),
    ("Best match", "relevance"),
];

/// The filter, post filter and sort shared by `browse` and `browse_packs`.
fn browse_vars(game: Value, name_key: &str, op: &str, words: &str, categories: &[&str], sort: &str, adult: bool) -> (Value, Value, Value) {
    let mut f = json!({"op": "AND"});
    f.as_object_mut().unwrap().extend(game.as_object().unwrap().clone());
    if !adult {
        f["adultContent"] = json!([{"value": false, "op": "EQUALS"}]);
    }
    let mut sort = sort;
    if words.split_whitespace().next().is_some() {
        let names: Vec<Value> = words.split_whitespace().map(|w| json!({"value": w, "op": op})).collect();
        f["filter"] = json!([{"op": "AND", name_key: names}]);
    } else if sort == "relevance" {
        sort = "downloads"; // nothing to match against
    }
    let post = if categories.is_empty() {
        let mut p = json!({"op": "AND"});
        p.as_object_mut().unwrap().extend(game.as_object().unwrap().clone());
        p
    } else {
        json!({"op": "OR", "categoryName": categories.iter().map(|c| json!({"value": c, "op": "EQUALS"})).collect::<Vec<_>>()})
    };
    (f, post, json!([{sort: {"direction": "DESC"}}]))
}

/// One page of Cyberpunk mods for the Browse tab: {"total", "categories": {name: count}, "mods": [...]}.
/// Mods must have every word in their name. Category counts are for the whole game (and the words), not the page.
pub fn browse(words: &str, categories: &[&str], sort: &str, page: i64, adult: bool, per_page: i64) -> Result<Value, String> {
    let game = json!({"gameDomainName": [{"value": GAME, "op": "EQUALS"}]});
    let (f, p, s) = browse_vars(game, "nameStemmed", "MATCHES", words, categories, sort, adult);
    let d = gql(
        r#"query($f: ModsFilter, $p: ModsFilter, $s: [ModsSort!], $o: Int, $n: Int) {
      mods(filter: $f, postFilter: $p, sort: $s, offset: $o, count: $n, facets: {categoryName: []}) { totalCount facetsData
        nodes { modId name summary author downloads endorsements updatedAt thumbnailUrl modCategory { name } } } }"#,
        "",
        json!({"f": f, "p": p, "s": s, "o": page * per_page, "n": per_page}),
    )?;
    let d = need(&d, "mods")?;
    let mut mods = vec![];
    for n in list(&d["nodes"]) {
        mods.push(json!({"id": int(&n["modId"])?, "name": n["name"], "summary": or(n.get("summary"), json!("")),
                         "author": or(n.get("author"), json!("")), "downloads": or(n.get("downloads"), json!(0)),
                         "endorsements": or(n.get("endorsements"), json!(0)), "updated": or(n.get("updatedAt"), json!("")),
                         "thumb": or(n.get("thumbnailUrl"), json!("")),
                         "category": get(&or(n.get("modCategory"), json!({})), "name", json!(""))}));
    }
    Ok(json!({"total": d["totalCount"], "categories": or(or(d.get("facetsData"), json!({})).get("categoryName"), json!({})),
              "mods": mods}))
}

/// One page of Cyberpunk collections (mod packs), in the same shape as browse(). Each entry has "slug" and "mod_count".
pub fn browse_packs(words: &str, categories: &[&str], sort: &str, page: i64, adult: bool, per_page: i64) -> Result<Value, String> {
    let game = json!({"gameDomain": [{"value": GAME, "op": "EQUALS"}]});
    let (f, p, s) = browse_vars(game, "name", "WILDCARD", words, categories, sort, adult);
    let d = gql(
        r#"query($f: CollectionsSearchFilter, $p: CollectionsSearchFilter, $s: [CollectionsSearchSort!], $o: Int, $n: Int) {
      collectionsV2(filter: $f, postFilter: $p, sort: $s, offset: $o, count: $n, facets: {categoryName: []}) { totalCount facetsData
        nodes { slug name summary endorsements totalDownloads updatedAt tileImage { thumbnailUrl(size: med) } user { name }
          category { name } latestPublishedRevision { modCount } } } }"#,
        "",
        json!({"f": f, "p": p, "s": s, "o": page * per_page, "n": per_page}),
    )?;
    let d = need(&d, "collectionsV2")?;
    let mods: Vec<Value> = list(&d["nodes"])
        .iter()
        .map(|n| {
            json!({"id": n["slug"], "slug": n["slug"], "name": n["name"], "summary": or(n.get("summary"), json!("")),
                   "author": get(&or(n.get("user"), json!({})), "name", json!("")),
                   "downloads": or(n.get("totalDownloads"), json!(0)), "endorsements": or(n.get("endorsements"), json!(0)),
                   "updated": or(n.get("updatedAt"), json!("")),
                   "thumb": or(or(n.get("tileImage"), json!({})).get("thumbnailUrl"), json!("")),
                   "category": get(&or(n.get("category"), json!({})), "name", json!("")),
                   "mod_count": or(or(n.get("latestPublishedRevision"), json!({})).get("modCount"), json!(0))})
        })
        .collect();
    Ok(json!({"total": d["totalCount"], "categories": or(or(d.get("facetsData"), json!({})).get("categoryName"), json!({})),
              "mods": mods}))
}

pub fn validate(api_key: &str) -> Result<Value, String> {
    http_json(&format!("{API}/users/validate.json"), None, &[("apikey", api_key)], 30)
}

pub fn download_url(api_key: &str, mod_id: i64, file_id: i64, key: Option<&str>, expires: Option<&str>) -> Result<String, String> {
    let q = match key.filter(|k| !k.is_empty()) {
        Some(k) => format!("?key={}&expires={}", quote(k, "", true), quote(expires.unwrap_or(""), "", true)),
        None => String::new(),
    };
    let links = http_json(&format!("{API}/games/{GAME}/mods/{mod_id}/files/{file_id}/download_link.json{q}"), None, &[("apikey", api_key)], 30)?;
    links[0]["URI"].as_str().map(String::from).ok_or_else(|| "Nexus gave no download link".into())
}

/// The page where a free user clicks "Slow download"; Nexus then hands the nxm:// link to this app.
pub fn files_page(mod_id: i64, file_id: i64) -> String {
    format!("https://www.nexusmods.com/{GAME}/mods/{mod_id}?tab=files&file_id={file_id}&nmm=1")
}

fn digits(s: &str) -> Option<i64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// One pasted line -> a job description, Ok(None) for blank/comment lines, an error when the link is unknown.
pub fn parse_link(text: &str) -> Result<Option<Value>, String> {
    static COLL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/collections/([A-Za-z0-9]+)(?:/revisions/([0-9]+))?").unwrap());
    static MODS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/mods/([0-9]+)").unwrap());
    static ARCHIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.(zip|7z|rar)$").unwrap());
    static REPO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w.-]+/[\w.-]+$").unwrap());
    let s = text.trim().trim_matches('"');
    if s.is_empty() || s.starts_with('#') {
        return Ok(None);
    }
    let unknown = || format!("Not a mod link I know: {s}");
    let u = urlparse(s);
    let q = parse_qs(&u.query);
    let host = u.netloc.to_lowercase();
    if u.scheme == "nxm" {
        let mut parts = vec![host.clone()];
        parts.extend(u.path.trim_matches('/').split('/').map(String::from));
        if parts[0] != GAME {
            return Err(format!("Not a Cyberpunk 2077 link: {s}"));
        }
        if parts.len() >= 5 && parts[1] == "mods" && parts[3] == "files" {
            let (mod_id, file_id) = (parts[2].parse::<i64>().map_err(|_| unknown())?, parts[4].parse::<i64>().map_err(|_| unknown())?);
            return Ok(Some(json!({"type": "nexus", "mod_id": mod_id, "file_id": file_id, "key": q.get("key"), "expires": q.get("expires")})));
        }
        if parts.len() >= 3 && parts[1] == "collections" {
            let rev = parts.get(4).and_then(|p| digits(p));
            return Ok(Some(json!({"type": "collection", "slug": parts[2], "revision": rev})));
        }
    }
    if host == "nexusmods.com" || host.ends_with(".nexusmods.com") {
        // not evilnexusmods.com
        if !u.path.to_lowercase().contains(GAME) {
            return Err(format!("Not a Cyberpunk 2077 link: {s}"));
        }
        if let Some(c) = COLL.captures(&u.path) {
            let rev = c.get(2).and_then(|r| r.as_str().parse::<i64>().ok());
            return Ok(Some(json!({"type": "collection", "slug": &c[1], "revision": rev})));
        }
        if let Some(m) = MODS.captures(&u.path) {
            let mod_id = m[1].parse::<i64>().map_err(|_| unknown())?;
            let file_id = q.get("file_id").and_then(|f| digits(f));
            return Ok(Some(json!({"type": "nexus", "mod_id": mod_id, "file_id": file_id})));
        }
    }
    if (u.scheme == "http" || u.scheme == "https") && ARCHIVE.is_match(&u.path) {
        if u.scheme != "https" {
            return Err(format!("Use an https:// link (plain http can be changed on the way): {s}"));
        }
        return Ok(Some(json!({"type": "url", "url": s})));
    }
    let segs: Vec<&str> = u.path.trim_matches('/').split('/').collect();
    if host == "github.com" && segs.len() >= 2 {
        let (owner, repo) = (segs[0], segs[1]);
        if !REPO.is_match(&format!("{owner}/{repo}")) || [owner, repo].iter().any(|p| *p == "." || *p == "..") {
            return Err(format!("Not a GitHub repository link: {s}"));
        }
        return Ok(Some(json!({"type": "github", "repo": format!("{owner}/{repo}")})));
    }
    let p = std::path::Path::new(s);
    if p.is_file() {
        let abs = std::path::absolute(p).map_err(|e| e.to_string())?;
        return Ok(Some(json!({"type": "file", "path": abs.to_string_lossy()})));
    }
    Err(unknown())
}
