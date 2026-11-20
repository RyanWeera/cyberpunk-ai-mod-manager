//! Links, file names, Nexus lookups and app updates (Python `tests/test_links.py`).
//! `Browse.test_numbers_and_dates` tests the window's `short` and `ago` and comes with the window.
use downloads::fetch::{self, Request, Response, ascii_name, set_mock};
use downloads::nexus::{self, files_page, newer_file, outdated, parse_link, pick_file};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Answers every request with `answer(request)` as JSON, and ends the mock when dropped.
struct Mocked;

impl Drop for Mocked {
    fn drop(&mut self) {
        set_mock(None);
    }
}

fn mock(answer: impl Fn(&Request) -> Value + Send + Sync + 'static) -> Mocked {
    set_mock(Some(Arc::new(move |r: &Request| {
        Ok(Response { status: 200, url: r.url.clone(), headers: vec![], body: answer(r).to_string().into_bytes() })
    })));
    Mocked
}

fn link(s: &str) -> Value {
    parse_link(s).expect("a known link").expect("not blank")
}

// test_links.py FileNames.test_names_are_plain_ascii
#[test]
fn test_names_are_plain_ascii() {
    assert_eq!(ascii_name("mod\u{FF02} --version \u{FF02}.zip"), "mod_ --version _.zip");
    assert!(ascii_name("\u{C4}rger Mod v1.2 (fix).7z").is_ascii());
    assert_eq!(ascii_name("..\\..\\x.zip"), "_.._x.zip");
    assert_eq!(ascii_name("..."), "download.zip");
}

// test_links.py FileNames.test_spaces_in_a_cdn_link_are_quoted
#[test]
fn test_spaces_in_a_cdn_link_are_quoted() {
    let asked = Arc::new(Mutex::new(vec![]));
    let seen = asked.clone();
    set_mock(Some(Arc::new(move |r: &Request| {
        seen.lock().unwrap().push(r.url.clone());
        Ok(Response {
            status: 200,
            url: "https://cf-files.nexusmods.com/3333/21307/Zenitex%20M50%20Gas%20Mask-21307-2.zip".into(),
            headers: vec![],
            body: b"ok".to_vec(),
        })
    })));
    let _m = Mocked;
    let d = testkit::TempDir::new();
    let dest = fetch::download("https://cf-files.nexusmods.com/3333/21307/Zenitex M50 Gas Mask-21307-2.zip?md5=a%2Fb&expires=1", d.path(), None, &[])
        .unwrap();
    assert_eq!(
        asked.lock().unwrap().as_slice(),
        ["https://cf-files.nexusmods.com/3333/21307/Zenitex%20M50%20Gas%20Mask-21307-2.zip?md5=a%2Fb&expires=1"]
    );
    assert_eq!(dest.file_name().unwrap(), "Zenitex M50 Gas Mask-21307-2.zip");
    assert_eq!(std::fs::read(&dest).unwrap(), b"ok");
}

// test_links.py AppUpdate.test_only_a_newer_release_counts
#[test]
fn test_only_a_newer_release_counts() {
    // Python patches VERSION to 1.1.0; here the tags are made from the real version.
    let v: Vec<u64> = shared::VERSION.split('.').map(|n| n.parse().unwrap()).collect();
    let newer = format!("v{}.{}.0-manual", v[0], v[1] + 10); // "2.10.0" > "2.0.0" only when compared as numbers
    let rel = |tag: &str, url: &str| {
        json!({"tag_name": tag, "assets": [{"name": "Notes.txt", "browser_download_url": "https://x/n"},
                                           {"name": "CP2077.AI.Mod.Manager.exe", "browser_download_url": url}]})
    };
    let exe = |tag: &str| format!("https://github.com/{}/releases/download/{tag}/CP2077.AI.Mod.Manager.exe", fetch::APP_REPO);
    for (tag, want) in [(format!("v{}-manual", shared::VERSION), None), ("v0.99.99-manual".to_string(), None), (newer.clone(), Some(exe(&newer)))] {
        let answer = json!([rel("v99.0.0", &exe("v99.0.0")), rel(&tag, &exe(&tag))]); // the automatic build is skipped
        let _m = mock(move |_| answer.clone());
        let got = fetch::app_latest().unwrap();
        assert_eq!(got, want.map(|url| json!({"tag": tag, "url": url})), "{tag}");
    }
    let answer = json!([rel("v99.0.0-manual", "https://evil.example/a.exe")]);
    let _m = mock(move |_| answer.clone());
    assert!(fetch::app_latest().is_err()); // an exe from anywhere else is not used
}

// test_links.py Outdated.test_reasons_a_mod_may_not_work
#[test]
fn test_reasons_a_mod_may_not_work() {
    assert_eq!(outdated("Better Cars", "published", "2025-01-01T00:00:00Z"), "");
    assert!(outdated("Better Cars", "hidden", "2025-01-01").contains("removed or hid"));
    assert!(outdated("Better Cars [OUTDATED]", "published", "2025-01-01").contains("name says"));
    assert_eq!(outdated("Fix for broken headlights", "published", "2025-01-01"), ""); // a fix, not a broken mod
    assert!(outdated("Better Cars", "published", "2022-05-07T10:00:00Z").contains("2.0"));
}

// test_links.py GithubCopy.test_only_the_same_version_of_the_mods_own_repo
#[test]
fn test_only_the_same_version_of_the_mods_own_repo() {
    let desc = "Needs [url=https://github.com/psiberx/cp2077-codeware]Codeware[/url]. Source: https://github.com/psiberx/\
                cp2077-equipment-ex and https://github.com/someone/other-tool and github.com/../equipment-x";
    let u = "https://github.com/psiberx/cp2077-equipment-ex/releases/download/v1.3.1/EquipmentEx-1.3.1.zip";
    let rel = Arc::new(Mutex::new(json!({"tag_name": "v1.3.1", "assets": [
        {"name": "EquipmentEx-1.3.1.zip", "browser_download_url": u},
        {"name": "EquipmentEx.pdb.zip", "browser_download_url": "p"}]})));
    let gets = Arc::new(Mutex::new(vec![]));
    let (r2, g2) = (rel.clone(), gets.clone());
    let _m = mock(move |r| {
        if r.url == nexus::GQL {
            json!({"data": {"mod": {"name": "Equipment-EX", "description": desc}}})
        } else {
            g2.lock().unwrap().push(r.url.clone());
            r2.lock().unwrap().clone()
        }
    });
    assert_eq!(nexus::github_copy(6945, "1.3.1").unwrap(), Some(json!({"repo": "psiberx/cp2077-equipment-ex", "url": u})));
    assert_eq!(gets.lock().unwrap().as_slice(), ["https://api.github.com/repos/psiberx/cp2077-equipment-ex/releases/latest"]); // not "../"
    assert_eq!(nexus::github_copy(6945, "1.3.0").unwrap(), None); // another version: the Nexus file is the one asked for
    rel.lock().unwrap()["assets"][0]["browser_download_url"] = json!("https://example.com/EquipmentEx-1.3.1.zip");
    assert_eq!(nexus::github_copy(6945, "1.3.1").unwrap(), None); // a file that is not on GitHub
    rel.lock().unwrap()["assets"][0]["browser_download_url"] = json!(u);
    rel.lock().unwrap()["assets"].as_array_mut().unwrap().push(json!({"name": "EquipmentEx-lite.zip", "browser_download_url": "l"}));
    assert_eq!(nexus::github_copy(6945, "1.3.1").unwrap(), None); // two variants: no guessing
}

// test_links.py Links.test_nexus_mod_page
#[test]
fn test_nexus_mod_page() {
    assert_eq!(link("https://www.nexusmods.com/cyberpunk2077/mods/107"), json!({"type": "nexus", "mod_id": 107, "file_id": null}));
    assert_eq!(link("https://www.nexusmods.com/cyberpunk2077/mods/107?tab=files&file_id=999")["file_id"], 999);
}

// test_links.py Links.test_collections
#[test]
fn test_collections() {
    let want = json!({"type": "collection", "slug": "iszwwe", "revision": null});
    assert_eq!(link("https://www.nexusmods.com/games/cyberpunk2077/collections/iszwwe"), want);
    assert_eq!(link("https://next.nexusmods.com/cyberpunk2077/collections/iszwwe"), want);
    assert_eq!(link("https://www.nexusmods.com/cyberpunk2077/collections/iszwwe/revisions/484")["revision"], 484);
    assert_eq!(link("nxm://cyberpunk2077/collections/iszwwe/revisions/12")["revision"], 12);
}

// test_links.py Links.test_nxm_file
#[test]
fn test_nxm_file() {
    assert_eq!(
        link("nxm://cyberpunk2077/mods/4198/files/1234?key=abc&expires=1700000000&user_id=5"),
        json!({"type": "nexus", "mod_id": 4198, "file_id": 1234, "key": "abc", "expires": "1700000000"})
    );
}

// test_links.py Links.test_other_sources
#[test]
fn test_other_sources() {
    assert_eq!(link("https://github.com/psiberx/cp2077-codeware/releases"), json!({"type": "github", "repo": "psiberx/cp2077-codeware"}));
    assert_eq!(link("https://github.com/a/b/releases/download/v1/b.zip")["type"], "url");
    assert_eq!(link("https://example.com/x.7z")["type"], "url");
    assert_eq!(parse_link("   "), Ok(None));
    assert_eq!(parse_link("# comment"), Ok(None));
}

// test_links.py Links.test_rejects
#[test]
fn test_rejects() {
    for bad in [
        "https://www.nexusmods.com/skyrimspecialedition/mods/1",
        "nxm://skyrim/mods/1/files/2",
        "hello",
        "http://example.com/x.zip", // plain http could be changed on the way
        "https://github.com/a/b\\..\\..\\x",
        "https://github.com/a/..",
        "https://github.com/../x", // plain names only
        "https://evilnexusmods.com/cyberpunk2077/mods/1", // not a Nexus host
    ] {
        assert!(parse_link(bad).is_err(), "{bad}");
    }
}

// test_links.py Links.test_file_choice
#[test]
fn test_file_choice() {
    let files = vec![
        json!({"fileId": 1, "name": "Main", "category": "OLD_VERSION", "primary": 0, "date": 1}),
        json!({"fileId": 2, "name": "Main", "category": "MAIN", "primary": 1, "date": 5}),
        json!({"fileId": 3, "name": "Patch", "category": "OPTIONAL", "primary": 0, "date": 9}),
        json!({"fileId": 4, "name": "Alt", "category": "MAIN", "primary": 0, "date": 7}),
    ];
    assert_eq!(pick_file(&files, None).unwrap()["fileId"], 2);
    assert_eq!(pick_file(&files, Some(3)).unwrap()["fileId"], 3);
    assert_eq!(newer_file(&files, 1).unwrap()["fileId"], 2);
    assert!(newer_file(&files, 2).is_none());
    assert!(files_page(1, 5).contains("file_id=5&nmm=1"));
}

// test_links.py Browse.test_query_filters_and_result
#[test]
fn test_query_filters_and_result() {
    let node = json!({"modId": "7", "name": "Nova", "summary": null, "author": "x", "downloads": 3, "endorsements": 1,
                      "updatedAt": "2026-01-01T00:00:00Z", "thumbnailUrl": null, "modCategory": {"name": "Visuals"}});
    let page = json!({"data": {"mods": {"totalCount": 41, "facetsData": {"categoryName": {"Visuals": 9}}, "nodes": [node]}}});
    let sent = Arc::new(Mutex::new(Value::Null));
    let s2 = sent.clone();
    let _m = mock(move |r| {
        *s2.lock().unwrap() = serde_json::from_slice::<Value>(r.body.as_deref().unwrap()).unwrap()["variables"].clone();
        page.clone()
    });
    let r = nexus::browse("nova lut", &["Visuals", "Audio"], "endorsements", 2, false, 20).unwrap();
    let v = sent.lock().unwrap().clone();
    assert_eq!(v["o"], 40);
    let words: Vec<&Value> = v["f"]["filter"][0]["nameStemmed"].as_array().unwrap().iter().map(|w| &w["value"]).collect();
    assert_eq!(words, ["nova", "lut"]);
    assert_eq!(v["f"]["adultContent"], json!([{"value": false, "op": "EQUALS"}]));
    let cats: Vec<&Value> = v["p"]["categoryName"].as_array().unwrap().iter().map(|c| &c["value"]).collect();
    assert_eq!(cats, ["Visuals", "Audio"]);
    assert_eq!(
        r["mods"][0],
        json!({"id": 7, "name": "Nova", "summary": "", "author": "x", "downloads": 3, "endorsements": 1,
               "updated": "2026-01-01T00:00:00Z", "thumb": "", "category": "Visuals"})
    );
    assert_eq!((&r["total"], &r["categories"]), (&json!(41), &json!({"Visuals": 9})));
    nexus::browse("", &[], "relevance", 0, true, 20).unwrap(); // no words: best match means nothing, so most downloaded
    let v = sent.lock().unwrap().clone();
    assert_eq!(v["s"], json!([{"downloads": {"direction": "DESC"}}]));
    assert!(v["f"].get("adultContent").is_none());
    assert_eq!(v["p"]["gameDomainName"][0]["value"], "cyberpunk2077");
}
