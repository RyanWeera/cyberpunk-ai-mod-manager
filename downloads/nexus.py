"""Nexus Mods: link parsing, mod and collection lookups (GraphQL, no key needed) and download links (v1 REST).

Free accounts: Nexus gives a download link only for a file the user clicked "Slow download" on in the
browser. That click sends an nxm:// link (with key + expires) to this app, and the rest is automatic.
Premium accounts get links straight from the API, so nothing has to be clicked.
"""
import os
import re
import urllib.parse
from concurrent.futures import ThreadPoolExecutor

from downloads.fetch import FW_BY_REPO, http_json

GAME, GAME_ID = "cyberpunk2077", 3333
GQL = "https://api.nexusmods.com/v2/graphql"
API = "https://api.nexusmods.com/v1"
GONE = ("ARCHIVED", "REMOVED", "DELETED", "OLD_VERSION")
GAME_2_0 = "2023-09-21"  # game update 2.0 changed scripts and game files: many mods not updated since then stopped working
OUTDATED = re.compile(r"outdated|broken|deprecated|obsolete|discontinued|abandoned|incompatible|not (?:compatible|working|updated)"
                      r"|no longer (?:works?|supported|updated|maintained)", re.IGNORECASE)


def outdated(name, status="published", updated=""):
    """Why a mod may not work with the game any more, or "" when nothing says so."""
    if (status or "published") != "published":
        return "the author removed or hid it on Nexus"
    if OUTDATED.search(name) and not re.search(r"fix|patch", name, re.IGNORECASE):  # "Fix for broken X" is a fix, not a broken mod
        return "its name says it is outdated or broken"
    if updated and updated < GAME_2_0:
        return f"it was last updated on {updated[:10]}, before game update 2.0 changed how mods work"
    return ""


def gql(query, api_key="", **variables):
    d = http_json(GQL, {"query": query, "variables": variables}, {"apikey": api_key} if api_key else None)
    if d.get("errors"):
        raise RuntimeError(d["errors"][0].get("message", "Nexus API error"))
    return d["data"]


def mod_info(mod_id, api_key=""):
    d = gql("""query($id: ID!, $game: ID!) {
      mod(modId: $id, gameId: $game) { name version status updatedAt
        modRequirements { nexusRequirements { nodes { modId modName gameId externalRequirement } } } }
      modFiles(modId: $id, gameId: $game) { fileId name version category primary date } }""",
            api_key, id=str(mod_id), game=str(GAME_ID))
    reqs = d["mod"]["modRequirements"]["nexusRequirements"]["nodes"]
    return {"name": d["mod"]["name"], "version": d["mod"]["version"], "files": d["modFiles"],
            "outdated": outdated(d["mod"]["name"], d["mod"].get("status"), d["mod"].get("updatedAt") or ""),
            "requires": [{"mod_id": int(r["modId"]), "name": r["modName"]} for r in reqs
                         if not r["externalRequirement"] and str(r["gameId"]) == str(GAME_ID) and int(r["modId"]) != int(mod_id)]}


def pick_file(files, file_id=None):
    """The asked-for file, else the newest primary MAIN file, else the newest file still listed."""
    if file_id:
        return next((f for f in files if int(f["fileId"]) == int(file_id)), None)
    pool = [f for f in files if f["category"] == "MAIN"] or [f for f in files if f["category"] not in GONE]
    return max(pool, key=lambda f: (f["primary"], f["date"]), default=None)


def newer_file(files, file_id):
    """A newer upload that replaces file_id (same file name first), or None when file_id is current."""
    cur = next((f for f in files if int(f["fileId"]) == int(file_id)), None)
    same = [f for f in files if cur and f["name"] == cur["name"] and f["category"] not in GONE]
    best = max(same, key=lambda f: f["date"], default=None) or pick_file(files)
    if best and int(best["fileId"]) != int(file_id) and (not cur or best["date"] > cur["date"]):
        return best
    return None


def collection(slug, revision=None, api_key=""):
    d = gql("""query($slug: String!, $rev: Int) {
      collectionRevision(slug: $slug, revision: $rev, domainName: "cyberpunk2077", viewAdultContent: true) {
        revisionNumber collection { name }
        modFiles { fileId optional file { name version mod { modId name status updatedAt } } } } }""", api_key, slug=slug, rev=revision)
    rev = d["collectionRevision"]
    return {"name": rev["collection"]["name"], "revision": rev["revisionNumber"],
            "files": [{"mod_id": int(f["file"]["mod"]["modId"]), "file_id": int(f["fileId"]), "optional": f["optional"],
                       "name": f["file"]["mod"]["name"], "version": f["file"]["version"],
                       "outdated": outdated(f["file"]["mod"]["name"], f["file"]["mod"].get("status"), f["file"]["mod"].get("updatedAt") or "")}
                      for f in rev["modFiles"] if f.get("file")]}


def mod_names(ids):
    """{mod id: {"name", "endorsements"}} for Cyberpunk mods, in one request. Hidden or deleted mods are left out."""
    d = gql("""query($ids: [CompositeDomainWithIdInput!]!, $n: Int) {
      legacyModsByDomain(ids: $ids, count: $n) { nodes { modId name endorsements } } }""",
            ids=[{"gameDomain": GAME, "modId": int(i)} for i in ids], n=len(ids))
    return {int(n["modId"]): n for n in d["legacyModsByDomain"]["nodes"]}


def mods_requiring(mod_id, most=1200):
    """[{"mod_id", "name"}] of the Cyberpunk mods that list mod_id as a requirement (patches and add-ons for it).
    Nexus gives at most 80 per request, so the pages are asked for at the same time; `most` caps very popular mods."""
    q = """query($id: ID!, $game: ID!, $o: Int) { mod(modId: $id, gameId: $game) { modRequirements {
      modsRequiringThisMod(offset: $o, count: 80) { totalCount nodes { modId modName gameId } } } } }"""
    page = lambda o: gql(q, id=str(mod_id), game=str(GAME_ID), o=o)["mod"]["modRequirements"]["modsRequiringThisMod"]
    first = page(0)
    nodes = list(first["nodes"])
    with ThreadPoolExecutor(6) as pool:
        for p in pool.map(page, range(80, min(first["totalCount"], most), 80)):
            nodes += p["nodes"]
    return [{"mod_id": int(n["modId"]), "name": n["modName"]} for n in nodes if str(n["gameId"]) == str(GAME_ID)]


def github_copy(mod_id, version):
    """{"repo", "url"} of the author's own GitHub release of a Nexus mod, so a free account needs no click; else None.
    The repo must be linked on the mod's Nexus page, carry a word of the mod's name and not be a framework. Its latest
    release must have the same version as the Nexus file and exactly one Windows archive (no guess between variants)."""
    want = version.lower().lstrip("v").strip()
    if not want:
        return None
    d = gql("""query($id: ID!, $game: ID!) { mod(modId: $id, gameId: $game) { name description } }""",
            id=str(mod_id), game=str(GAME_ID))["mod"]
    words = [w for w in re.findall(r"[a-z0-9]{4,}", d["name"].lower()) if w not in ("cyberpunk", "2077", "mods")]
    for repo in dict.fromkeys(re.findall(r"github\.com/(\w[\w.-]*/\w[\w.-]*)", d.get("description") or "")):
        repo = repo.removesuffix(".git")
        name = re.sub(r"[^a-z0-9]", "", repo.split("/")[1].lower())
        if repo.lower() in FW_BY_REPO or not any(w in name for w in words):
            continue
        try:
            rel = http_json(f"https://api.github.com/repos/{repo}/releases/latest")
        except OSError:  # no releases, or GitHub's hourly limit: the Nexus click still works
            continue
        assets = [a for a in rel.get("assets", []) if a["name"].lower().endswith((".zip", ".7z"))
                  and not re.search(r"pdb|symbol|mac|darwin|linux", a["name"], re.IGNORECASE)]
        if (rel.get("tag_name", "").lower().lstrip("v").strip() == want and len(assets) == 1
                and assets[0]["browser_download_url"].startswith("https://github.com/")):
            return {"repo": repo, "url": assets[0]["browser_download_url"]}
    return None


def collection_info(slug):
    """{"name", "endorsements"} of a Cyberpunk collection, without its file list. RuntimeError when it is gone."""
    return gql("""query($s: String!) { collection(slug: $s, domainName: "%s", viewAdultContent: true) {
      name endorsements } }""" % GAME, s=slug)["collection"]


def search(words):
    """Cyberpunk mods with any of `words` in their name, best first. Mods with every word come first, the most
    endorsed of them first (the original before its translations); then partial matches in Nexus relevance order."""
    q = """query($w: [BaseFilterValue!], $sort: [ModsSort!]) { mods(filter: {op: AND,
      gameDomainName: [{value: "%s", op: EQUALS}], filter: [{op: OR, nameStemmed: $w}]}, sort: $sort, count: 20) {
      nodes { modId name endorsements } } }""" % GAME
    found = {}
    for sort in ("relevance", "endorsements"):
        for rank, n in enumerate(gql(q, w=[{"value": x, "op": "MATCHES"} for x in words], sort=[{sort: {"direction": "DESC"}}])["mods"]["nodes"]):
            found.setdefault(n["modId"], {**n, "rank": rank if sort == "relevance" else 99})

    def key(n):
        hits = sum(x.lower() in n["name"].lower() for x in words)
        return (-hits, 0, -n["endorsements"]) if hits == len(words) else (-hits, 1, n["rank"])
    return sorted(found.values(), key=key)


SORTS = {"Most downloaded": "downloads", "Most endorsed": "endorsements", "Recently updated": "updatedAt", "Newest": "createdAt",
         "Best match": "relevance"}


def browse(words="", categories=(), sort="downloads", page=0, adult=False, per_page=20):
    """One page of Cyberpunk mods for the Browse tab: {"total", "categories": {name: count}, "mods": [...]}.
    Mods must have every word in their name. Category counts are for the whole game (and the words), not the page."""
    game = {"gameDomainName": [{"value": GAME, "op": "EQUALS"}]}
    f = {"op": "AND", **game}
    if not adult:
        f["adultContent"] = [{"value": False, "op": "EQUALS"}]
    if words.split():
        f["filter"] = [{"op": "AND", "nameStemmed": [{"value": w, "op": "MATCHES"} for w in words.split()]}]
    elif sort == "relevance":
        sort = "downloads"  # nothing to match against
    post = {"op": "OR", "categoryName": [{"value": c, "op": "EQUALS"} for c in categories]} if categories else {"op": "AND", **game}
    d = gql("""query($f: ModsFilter, $p: ModsFilter, $s: [ModsSort!], $o: Int, $n: Int) {
      mods(filter: $f, postFilter: $p, sort: $s, offset: $o, count: $n, facets: {categoryName: []}) { totalCount facetsData
        nodes { modId name summary author downloads endorsements updatedAt thumbnailUrl modCategory { name } } } }""",
            f=f, p=post, s=[{sort: {"direction": "DESC"}}], o=page * per_page, n=per_page)["mods"]
    return {"total": d["totalCount"], "categories": (d.get("facetsData") or {}).get("categoryName") or {},
            "mods": [{"id": int(n["modId"]), "name": n["name"], "summary": n.get("summary") or "", "author": n.get("author") or "",
                      "downloads": n.get("downloads") or 0, "endorsements": n.get("endorsements") or 0, "updated": n.get("updatedAt") or "",
                      "thumb": n.get("thumbnailUrl") or "", "category": (n.get("modCategory") or {}).get("name", "")} for n in d["nodes"]]}


def browse_packs(words="", categories=(), sort="downloads", page=0, adult=False, per_page=20):
    """One page of Cyberpunk collections (mod packs), in the same shape as browse(). Each entry has "slug" and "mod_count"."""
    game = {"gameDomain": [{"value": GAME, "op": "EQUALS"}]}
    f = {"op": "AND", **game}
    if not adult:
        f["adultContent"] = [{"value": False, "op": "EQUALS"}]
    if words.split():
        f["filter"] = [{"op": "AND", "name": [{"value": w, "op": "WILDCARD"} for w in words.split()]}]
    elif sort == "relevance":
        sort = "downloads"
    post = {"op": "OR", "categoryName": [{"value": c, "op": "EQUALS"} for c in categories]} if categories else {"op": "AND", **game}
    d = gql("""query($f: CollectionsSearchFilter, $p: CollectionsSearchFilter, $s: [CollectionsSearchSort!], $o: Int, $n: Int) {
      collectionsV2(filter: $f, postFilter: $p, sort: $s, offset: $o, count: $n, facets: {categoryName: []}) { totalCount facetsData
        nodes { slug name summary endorsements totalDownloads updatedAt tileImage { thumbnailUrl(size: med) } user { name }
          category { name } latestPublishedRevision { modCount } } } }""",
            f=f, p=post, s=[{sort: {"direction": "DESC"}}], o=page * per_page, n=per_page)["collectionsV2"]
    return {"total": d["totalCount"], "categories": (d.get("facetsData") or {}).get("categoryName") or {},
            "mods": [{"id": n["slug"], "slug": n["slug"], "name": n["name"], "summary": n.get("summary") or "",
                      "author": (n.get("user") or {}).get("name", ""), "downloads": n.get("totalDownloads") or 0,
                      "endorsements": n.get("endorsements") or 0, "updated": n.get("updatedAt") or "",
                      "thumb": (n.get("tileImage") or {}).get("thumbnailUrl") or "", "category": (n.get("category") or {}).get("name", ""),
                      "mod_count": (n.get("latestPublishedRevision") or {}).get("modCount") or 0} for n in d["nodes"]]}


def validate(api_key):
    return http_json(f"{API}/users/validate.json", headers={"apikey": api_key})


def download_url(api_key, mod_id, file_id, key=None, expires=None):
    q = "?" + urllib.parse.urlencode({"key": key, "expires": expires}) if key else ""
    links = http_json(f"{API}/games/{GAME}/mods/{mod_id}/files/{file_id}/download_link.json{q}", headers={"apikey": api_key})
    return links[0]["URI"]


def files_page(mod_id, file_id):
    """The page where a free user clicks "Slow download"; Nexus then hands the nxm:// link to this app."""
    return f"https://www.nexusmods.com/{GAME}/mods/{mod_id}?tab=files&file_id={file_id}&nmm=1"


def parse_link(text):
    """One pasted line -> a job description dict, None for blank/comment lines, ValueError when unknown."""
    s = text.strip().strip('"')
    if not s or s.startswith("#"):
        return None
    u = urllib.parse.urlparse(s)
    q = dict(urllib.parse.parse_qsl(u.query))
    if u.scheme == "nxm":
        parts = [u.netloc.lower()] + u.path.strip("/").split("/")
        if parts[0] != GAME:
            raise ValueError(f"Not a Cyberpunk 2077 link: {s}")
        if len(parts) >= 5 and parts[1] == "mods" and parts[3] == "files":
            return {"type": "nexus", "mod_id": int(parts[2]), "file_id": int(parts[4]), "key": q.get("key"), "expires": q.get("expires")}
        if len(parts) >= 3 and parts[1] == "collections":
            return {"type": "collection", "slug": parts[2], "revision": int(parts[4]) if len(parts) > 4 and parts[4].isdigit() else None}
    host = u.netloc.lower()
    if host == "nexusmods.com" or host.endswith(".nexusmods.com"):  # not evilnexusmods.com
        if GAME not in u.path.lower():
            raise ValueError(f"Not a Cyberpunk 2077 link: {s}")
        c = re.search(r"/collections/([A-Za-z0-9]+)(?:/revisions/(\d+))?", u.path)
        if c:
            return {"type": "collection", "slug": c[1], "revision": int(c[2]) if c[2] else None}
        m = re.search(r"/mods/(\d+)", u.path)
        if m:
            return {"type": "nexus", "mod_id": int(m[1]), "file_id": int(q["file_id"]) if q.get("file_id", "").isdigit() else None}
    if u.scheme in ("http", "https") and re.search(r"\.(zip|7z|rar)$", u.path, re.I):
        if u.scheme != "https":
            raise ValueError(f"Use an https:// link (plain http can be changed on the way): {s}")
        return {"type": "url", "url": s}
    if host == "github.com" and len(u.path.strip("/").split("/")) >= 2:
        owner, repo = u.path.strip("/").split("/")[:2]
        if not re.fullmatch(r"[\w.-]+/[\w.-]+", f"{owner}/{repo}") or {owner, repo} & {".", ".."}:
            raise ValueError(f"Not a GitHub repository link: {s}")
        return {"type": "github", "repo": f"{owner}/{repo}"}
    if os.path.isfile(s):
        return {"type": "file", "path": os.path.abspath(s)}
    raise ValueError(f"Not a mod link I know: {s}")
