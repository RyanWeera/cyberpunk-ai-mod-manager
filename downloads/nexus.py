"""Nexus Mods: link parsing, mod and collection lookups (GraphQL, no key needed) and download links (v1 REST).

Free accounts: Nexus gives a download link only for a file the user clicked "Slow download" on in the
browser. That click sends an nxm:// link (with key + expires) to this app, and the rest is automatic.
Premium accounts get links straight from the API, so nothing has to be clicked.
"""
import os
import re
import urllib.parse

from downloads.fetch import http_json

GAME, GAME_ID = "cyberpunk2077", 3333
GQL = "https://api.nexusmods.com/v2/graphql"
API = "https://api.nexusmods.com/v1"
GONE = ("ARCHIVED", "REMOVED", "DELETED", "OLD_VERSION")


def gql(query, api_key="", **variables):
    d = http_json(GQL, {"query": query, "variables": variables}, {"apikey": api_key} if api_key else None)
    if d.get("errors"):
        raise RuntimeError(d["errors"][0].get("message", "Nexus API error"))
    return d["data"]


def mod_info(mod_id, api_key=""):
    d = gql("""query($id: ID!, $game: ID!) {
      mod(modId: $id, gameId: $game) { name version
        modRequirements { nexusRequirements { nodes { modId modName gameId externalRequirement } } } }
      modFiles(modId: $id, gameId: $game) { fileId name version category primary date } }""",
            api_key, id=str(mod_id), game=str(GAME_ID))
    reqs = d["mod"]["modRequirements"]["nexusRequirements"]["nodes"]
    return {"name": d["mod"]["name"], "version": d["mod"]["version"], "files": d["modFiles"],
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
        modFiles { fileId optional file { name version mod { modId name } } } } }""", api_key, slug=slug, rev=revision)
    rev = d["collectionRevision"]
    return {"name": rev["collection"]["name"], "revision": rev["revisionNumber"],
            "files": [{"mod_id": int(f["file"]["mod"]["modId"]), "file_id": int(f["fileId"]), "optional": f["optional"],
                       "name": f["file"]["mod"]["name"], "version": f["file"]["version"]} for f in rev["modFiles"] if f.get("file")]}


def mod_names(ids):
    """{mod id: {"name", "endorsements"}} for Cyberpunk mods, in one request. Hidden or deleted mods are left out."""
    d = gql("""query($ids: [CompositeDomainWithIdInput!]!, $n: Int) {
      legacyModsByDomain(ids: $ids, count: $n) { nodes { modId name endorsements } } }""",
            ids=[{"gameDomain": GAME, "modId": int(i)} for i in ids], n=len(ids))
    return {int(n["modId"]): n for n in d["legacyModsByDomain"]["nodes"]}


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
