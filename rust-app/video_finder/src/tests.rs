//! Find mod from video, offline: pages and Nexus answers are canned (test_video.py).
use super::*;
use downloads::fetch::Response;
use std::sync::Mutex;

/// Canned pages by URL, the URLs asked for, and the number of Nexus GraphQL calls.
#[derive(Default)]
struct Net {
    pages: HashMap<String, Response>,
    asked: Vec<String>,
    gql: usize,
}

/// Resets the network and DNS mocks when a test ends, also when it fails.
struct Reset;

impl Drop for Reset {
    fn drop(&mut self) {
        fetch::set_mock(None);
        set_resolver(None);
    }
}

fn ok(body: &str) -> Response {
    Response { status: 200, body: body.as_bytes().to_vec(), ..Default::default() }
}

fn moved(to: &str) -> Response {
    Response { status: 302, headers: vec![("Location".into(), to.into())], ..Default::default() }
}

fn resolves_to(addr: &str) -> Resolver {
    let ip: IpAddr = addr.parse().unwrap();
    Arc::new(move |_| Ok(vec![ip]))
}

fn fake_gql(body: &[u8]) -> Value {
    let b: Value = serde_json::from_slice(body).unwrap();
    let (q, v) = (b["query"].as_str().unwrap(), &b["variables"]);
    if q.contains("legacyModsByDomain") {
        let nodes: Vec<Value> = v["ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["modId"] == 4714)
            .map(|_| json!({"modId": 4714, "name": "Stand Still Please", "endorsements": 6602}))
            .collect();
        return json!({"data": {"legacyModsByDomain": {"nodes": nodes}}});
    }
    if q.contains("collection(slug") {
        if v["s"] != "khbuxr" {
            return json!({"errors": [{"message": "Collection not found"}]});
        }
        return json!({"data": {"collection": {"name": "Spectr Collection", "endorsements": 11}}});
    }
    json!({"data": {"mods": {"nodes": [{"modId": 2, "name": "Nova LUT 4.0 - Russian", "endorsements": 10},
                                       {"modId": 3, "name": "Nova City Lights", "endorsements": 900},
                                       {"modId": 1, "name": "Nova LUT 4.0", "endorsements": 5000}]}}})
}

/// The Finder setUp: canned pages, a public address for every name, and fake Nexus answers.
fn net(pages: Vec<(&str, Response)>) -> (Arc<Mutex<Net>>, Reset) {
    let n = Arc::new(Mutex::new(Net { pages: pages.into_iter().map(|(u, r)| (u.to_string(), r)).collect(), ..Default::default() }));
    let m = n.clone();
    fetch::set_mock(Some(Arc::new(move |req: &Request| {
        let mut n = m.lock().unwrap();
        if req.url == nexus::GQL {
            n.gql += 1;
            return Ok(ok(&fake_gql(req.body.as_deref().unwrap_or(b"")).to_string()));
        }
        n.asked.push(req.url.clone());
        n.pages.get(&req.url).cloned().ok_or(format!("no page for {}", req.url))
    })));
    set_resolver(Some(resolves_to("93.184.215.14")));
    (n, Reset)
}

fn youtube(details: Value) -> Response {
    ok(&format!("<script>var ytInitialPlayerResponse = {};var meta = {{}};</script>", json!({"videoDetails": details})))
}

// test_video.py Finder.test_youtube_ids
#[test]
fn test_youtube_ids() {
    for url in [
        "https://youtu.be/8Yexbiw0T28?si=x",
        "https://www.youtube.com/watch?v=8Yexbiw0T28&t=5",
        "https://m.youtube.com/shorts/8Yexbiw0T28",
        "https://youtube.com/live/8Yexbiw0T28",
    ] {
        assert_eq!(youtube_id(url).as_deref(), Some("8Yexbiw0T28"), "{url}");
    }
    assert_eq!(youtube_id("https://www.tiktok.com/@a/video/1"), None);
}

// test_video.py Finder.test_youtube_links_win
#[test]
fn test_youtube_links_win() {
    let desc = "0:00 Intro\n1:23 Better Vehicle Handling\nMods:\nhttps://www.nexusmods.com/cyberpunk2077/mods/4714?tab=files\n\
                nexusmods.com/skyrim/mods/1\nhttps://github.com/psiberx/cp2077-codeware/releases\n\
                https://www.nexusmods.com/cyberpunk2077/mods/4714.\nhttps://next.nexusmods.com/cyberpunk2077/collections/khbuxr\n\
                https://www.nexusmods.com/games/cyberpunk2077/collections/gone11";
    let (n, _reset) = net(vec![(
        "https://www.youtube.com/watch?v=8Yexbiw0T28",
        youtube(json!({"title": "Top mods", "shortDescription": desc})),
    )]);
    let (title, rows) = find("https://youtu.be/8Yexbiw0T28").unwrap();
    assert_eq!(title, "Top mods");
    let why = "Linked in the video";
    assert_eq!(
        rows,
        vec![
            json!({"name": "Stand Still Please", "url": "https://www.nexusmods.com/cyberpunk2077/mods/4714", "endorsements": 6602, "why": why}),
            json!({"name": "psiberx/cp2077-codeware", "url": "https://github.com/psiberx/cp2077-codeware", "endorsements": null, "why": why}),
            json!({"name": "Spectr Collection", "url": "https://www.nexusmods.com/games/cyberpunk2077/collections/khbuxr", "endorsements": 11,
                   "why": why}),
            json!({"name": "Collection gone11", "url": "https://www.nexusmods.com/games/cyberpunk2077/collections/gone11",
                   "endorsements": null, "why": why}),
        ]
    );
    assert_eq!(n.lock().unwrap().gql, 3); // names + 2 collections; a linked Nexus mod means no name search
}

// test_video.py Finder.test_name_search_when_nothing_is_linked
#[test]
fn test_name_search_when_nothing_is_linked() {
    let (_n, _reset) = net(vec![]);
    let (title, rows) = find("Nova LUT 4.0 | best reshade").unwrap();
    assert_eq!(title, "");
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Nova LUT 4.0", "Nova LUT 4.0 - Russian", "Nova City Lights"]);
    assert_eq!(rows[0]["why"], "Name matches \"Nova LUT\"");
    assert_eq!(rows[0]["url"], "https://www.nexusmods.com/cyberpunk2077/mods/1");
}

// test_video.py Finder.test_phrases
#[test]
fn test_phrases() {
    assert_eq!(
        phrases("Cyberpunk 2077 - Realistic Driving Mod | 4K", "0:00 Intro\n2:10 Nova LUT (night)"),
        [vec!["Realistic", "Driving"], vec!["Nova", "LUT"], vec!["night"]]
    );
    assert_eq!(phrases("", "#cyberpunk @someone https://x.y/z\nBlackwall Unleashed!"), [vec!["Blackwall", "Unleashed"]]);
}

// test_video.py Finder.test_tiktok_short_link
#[test]
fn test_tiktok_short_link() {
    let vid = "7590235962194627856";
    let long = format!("https://www.tiktok.com/@u/video/{vid}?lang=en");
    let state = json!({"source": {"data": {format!("/embed/{vid}"): {"videoData": {"itemInfos": {"text": "Native Unleash Blackwall #cp2077"}}}}}});
    let embed = format!("https://www.tiktok.com/embed/{vid}");
    let (_n, _reset) = net(vec![
        ("https://vm.tiktok.com/ZM123/", moved(&long)),
        (&long, ok("")),
        (&embed, ok(&format!(r#"<script id="__FRONTITY_CONNECT_STATE__" type="application/json">{state}</script>"#))),
    ]);
    assert_eq!(video_text("https://vm.tiktok.com/ZM123/").unwrap(), ("".into(), "Native Unleash Blackwall #cp2077".into()));
}

// test_video.py Finder.test_instagram_caption
#[test]
fn test_instagram_caption() {
    let (_n, _reset) = net(vec![(
        "https://www.instagram.com/p/AB_c1/embed/captioned/",
        ok(r#"<div class="Caption"><a class="CaptionUsername" href="/u">someone</a><br />Mod: Better Lights &amp; more<br/><a href="/t">#cyberpunk</a></div><div class="CaptionComments">"#),
    )]);
    assert_eq!(
        video_text("https://www.instagram.com/reel/AB_c1/?igsh=x").unwrap(),
        ("".into(), "Mod: Better Lights & more\n#cyberpunk".into())
    );
}

// test_video.py Finder.test_other_site_meta
#[test]
fn test_other_site_meta() {
    let (_n, _reset) = net(vec![(
        "https://x.com/a/status/1",
        ok(r#"<title>X</title><meta property="og:title" content="Better &amp; Brighter"><meta name="description" content="A mod">"#),
    )]);
    assert_eq!(video_text("https://x.com/a/status/1").unwrap(), ("Better & Brighter".into(), "A mod".into()));
}

// test_video.py Finder.test_hostile_page_stays_fast_and_links_need_no_https
#[test]
fn test_hostile_page_stays_fast_and_links_need_no_https() {
    let long = format!(r#"<meta {}><meta property="og:title" content="T">"#, "a".repeat(200_000));
    let (n, _reset) = net(vec![
        ("https://x.com/long", ok(&long)),
        ("https://www.youtube.com/watch?v=dQw4w9WgXcQ", youtube(json!({"title": "", "shortDescription": "a.".repeat(100_000)}))),
    ]);
    assert_eq!(video_text("https://x.com/long").unwrap(), ("T".into(), "".into()));
    n.lock().unwrap().asked.clear();
    assert_eq!(find("youtu.be/dQw4w9WgXcQ").unwrap().1, Vec::<Value>::new());
    assert_eq!(find("www.nexusmods.com/cyberpunk2077/mods/4714").unwrap().1[0]["name"], "Stand Still Please");
    // read as https://youtu.be/dQw4w9WgXcQ; a Nexus link is read as it is, not opened as a video
    assert_eq!(n.lock().unwrap().asked, ["https://www.youtube.com/watch?v=dQw4w9WgXcQ"]);
}

// test_video.py Finder.test_hostile_instagram_page_stays_fast
#[test]
fn test_hostile_instagram_page_stays_fast() {
    let url = "https://www.instagram.com/p/x/embed/captioned/";
    let (n, _reset) = net(vec![(url, ok(&r#"<div class="Caption">"#.repeat(100_000)))]);
    assert_eq!(video_text("https://www.instagram.com/p/x/").unwrap(), ("".into(), "".into()));
    let page = format!(r#"<div class="Caption">{}<div class="CaptionComments">"#, "<".repeat(1_000_000));
    n.lock().unwrap().pages.insert(url.into(), ok(&page));
    assert_eq!(video_text("https://www.instagram.com/p/x/").unwrap().1.chars().count(), 20_000);
}

/// A network that records every request and answers with `answer`.
fn recorder(answer: fn(&str) -> Response) -> Arc<Mutex<Vec<String>>> {
    let asked = Arc::new(Mutex::new(vec![]));
    let a = asked.clone();
    fetch::set_mock(Some(Arc::new(move |req: &Request| {
        a.lock().unwrap().push(req.url.clone());
        Ok(answer(&req.url))
    })));
    asked
}

// test_video.py PublicPagesOnly.test_this_pc_and_the_local_network_are_refused
#[test]
fn test_this_pc_and_the_local_network_are_refused() {
    let _reset = Reset;
    let asked = recorder(|_| ok("secret"));
    // "fe80::1%3" in Python: an IpAddr has no scope id
    for addr in ["127.0.0.1", "10.1.2.3", "192.168.0.10", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "::ffff:127.0.0.1", "fe80::1"] {
        set_resolver(Some(resolves_to(addr)));
        let e = page("https://video.example/v", LIMIT).unwrap_err();
        assert!(e.contains("local network"), "{addr}: {e}");
    }
    assert!(asked.lock().unwrap().is_empty());
    for bad in ["file:///C:/x", "ftp://video.example/x", "https:///x"] {
        assert!(public(bad).unwrap_err().contains("http"), "{bad}");
    }
}

// test_video.py PublicPagesOnly.test_public_pages_load_and_redirects_are_checked
#[test]
fn test_public_pages_load_and_redirects_are_checked() {
    let _reset = Reset;
    set_resolver(Some(Arc::new(|host: &str| Ok(vec![if host == "localhost" { "127.0.0.1" } else { "93.184.215.14" }.parse().unwrap()]))));
    recorder(|_| ok("hi"));
    assert_eq!(page("https://video.example/v", LIMIT).unwrap(), ("hi".into(), "https://video.example/v".into()));
    let asked = recorder(|u| if u == "https://video.example/v" { moved("http://localhost:8080/admin") } else { ok("admin") });
    assert!(page("https://video.example/v", LIMIT).unwrap_err().contains("local network"));
    assert_eq!(*asked.lock().unwrap(), ["https://video.example/v"]);
    recorder(|u| if u == "https://video.example/v" { moved("ftp://video.example/x") } else { ok("") });
    assert!(page("https://video.example/v", LIMIT).unwrap_err().contains("http"));
    recorder(|u| if u == "https://video.example/v" { moved("https://www.video.example/v") } else { ok("moved") });
    assert_eq!(page("https://video.example/v", LIMIT).unwrap(), ("moved".into(), "https://www.video.example/v".into()));
}
