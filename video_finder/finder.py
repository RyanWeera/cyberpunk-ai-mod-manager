"""Find the Cyberpunk 2077 mods a video shows, from the text around it: mod links in the description first,
else a Nexus name search with the title, the chapter titles or the caption. YouTube gives the most text
(full description with links and chapters); TikTok and Instagram give the caption; other sites give the
page title and summary. The video itself is never watched."""
import html
import ipaddress
import json
import re
import socket
import urllib.parse
import urllib.request

from downloads import nexus

# SOCS=CAI: "reject all" on Google's cookie consent page, so YouTube in the EU shows the video page itself.
HEADERS = {"User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64)", "Accept-Language": "en-US,en;q=0.9", "Cookie": "SOCS=CAI"}
LINK = re.compile(r"(?:https?://)?(?:[\w-]+\.)*(?:nexusmods\.com|github\.com)/[^\s<>\"'()\[\]]+", re.I)
CHAPTER = re.compile(r"^\W*(?:\d{1,2}:)?\d{1,2}:\d{2}\W*(.+)$", re.M)
NOISE = set("""cyberpunk cyberpunk2077 2077 cp2077 cp77 mod mods modded modding showcase nexus nexusmods the a an this that these
my your you i is are was in on at for of to with and or by how install installing installed guide tutorial best top new
most pc 4k 8k rtx gameplay game now just it its makes make look looks like must have need needs only update patch version
fyp foryou foryoupage shorts short video intro outro part ultra""".split())


def public(url):
    """Refuse a link to this PC or the local network (a router page, another app's local server): the link comes
    from the user, an AI app or a redirect, and only a public web page belongs here."""
    u = urllib.parse.urlparse(url)
    if u.scheme not in ("http", "https") or not u.hostname:
        raise ValueError("Paste an http:// or https:// link")
    for *_, (addr, *_) in socket.getaddrinfo(u.hostname, None):
        ip = ipaddress.ip_address(addr)
        if not (getattr(ip, "ipv4_mapped", None) or ip).is_global:
            raise ValueError(f"{u.hostname} is on this PC or your local network: paste a link to a public video page")


class _PublicRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        public(newurl)
        return super().redirect_request(req, fp, code, msg, headers, newurl)


OPENER = urllib.request.build_opener(_PublicRedirects)


def page(url, limit=8_000_000):
    """The page at url as text, and the address it ended at after redirects."""
    # ponytail: the name is looked up again to connect, so a DNS server that answers differently the second time
    # (rebinding) gets through; connect to the checked address if that ever matters
    public(url)
    with OPENER.open(urllib.request.Request(url, headers=HEADERS), timeout=30) as r:
        return r.read(limit).decode("utf-8", "replace"), r.url


def youtube_id(url):
    u = urllib.parse.urlparse(url)
    host = u.netloc.lower().removeprefix("www.").removeprefix("m.")
    if host == "youtu.be":
        return u.path.strip("/")[:11] or None
    if host.endswith("youtube.com"):
        v = urllib.parse.parse_qs(u.query).get("v")
        m = re.match(r"/(?:shorts|live|embed)/([\w-]{11})", u.path)
        return v[0][:11] if v else m and m[1]
    return None


def _json_after(text, marker):
    """The JSON value that starts after `marker` in text, or None."""
    i = text.find(marker)
    if i < 0:
        return None
    try:
        return json.JSONDecoder().raw_decode(text, text.index("{", i))[0]
    except ValueError:
        return None


def _meta(text):
    """{name: content} of a page's <meta> tags (og:title, description, ...)."""
    out = {}
    for tag in re.findall(r"<meta\b[^<>]*>", text, re.I):
        a = {k.lower(): html.unescape(v) for k, v in re.findall(r'(?<![\w:-])([\w:-]+)\s*=\s*"([^"]*)"', tag)}
        out.setdefault(a.get("property") or a.get("name", ""), a.get("content", ""))
    return out


def video_text(url):
    """(title, description or caption) of the video at url."""
    host = urllib.parse.urlparse(url).netloc.lower()
    vid = youtube_id(url)
    if vid:
        d = _json_after(page(f"https://www.youtube.com/watch?v={vid}")[0], "ytInitialPlayerResponse")
        v = (d or {}).get("videoDetails") or {}
        if not v:
            raise RuntimeError("YouTube did not show this video (private, removed or age-restricted?)")
        return v.get("title", ""), v.get("shortDescription", "")
    if host.endswith("tiktok.com"):
        m = re.search(r"/video/(\d+)", url) or re.search(r"/video/(\d+)", page(url, 200_000)[1])  # short links redirect
        if not m:
            raise RuntimeError("This TikTok link has no video number in it")
        d = _json_after(page(f"https://www.tiktok.com/embed/{m[1]}")[0], 'id="__FRONTITY_CONNECT_STATE__"') or {}
        info = d.get("source", {}).get("data", {}).get(f"/embed/{m[1]}", {}).get("videoData", {}).get("itemInfos", {})
        return "", info.get("text", "")
    m = re.search(r"/(?:p|reels?|tv)/([\w-]+)", urllib.parse.urlparse(url).path)
    if host.endswith("instagram.com") and m:
        text = page(f"https://www.instagram.com/p/{m[1]}/embed/captioned/")[0]
        i = text.find('<div class="Caption"')  # str.find, not a pattern: the page can be 8 MB
        j = text.find('<div class="CaptionComments"', i)
        cap = text[text.find(">", i) + 1:j][:20_000] if 0 <= i < j else ""  # a caption has at most 2,200 letters
        cap = re.sub(r'<a class="CaptionUsername".*?</a>', "", cap, flags=re.S)
        return "", html.unescape(re.sub(r"<br\s*/?>", "\n", re.sub(r"<(?!br)[^<>]*>", "", cap))).strip()
    text = page(url)[0]
    meta = _meta(text)
    t = re.search(r"<title[^<>]*>([^<]*)</title>", text, re.I)
    title = meta.get("og:title") or meta.get("twitter:title") or (html.unescape(t[1]) if t else "")
    return title.strip(), (meta.get("og:description") or meta.get("description") or "").strip()


def links(text):
    """Mods linked in the text: Nexus mods and collections for Cyberpunk, and GitHub repositories."""
    seen, rows, ids = set(), [], []
    for raw in LINK.findall(text):
        url = raw.rstrip(".,;:!?")
        try:
            link = nexus.parse_link(url if url.lower().startswith("http") else "https://" + url)
        except ValueError:  # another game, a user page, the game's front page...
            continue
        key = (link["type"], link.get("mod_id") or link.get("slug") or link.get("repo"))
        if link["type"] not in ("nexus", "collection", "github") or key in seen:
            continue
        seen.add(key)
        if link["type"] == "nexus":
            ids.append(link["mod_id"])
            rows.append({"name": f"Nexus mod {link['mod_id']}", "mod_id": link["mod_id"],
                         "url": f"https://www.nexusmods.com/{nexus.GAME}/mods/{link['mod_id']}"})
        elif link["type"] == "collection":
            try:
                c = nexus.collection_info(link["slug"]) or {}
            except RuntimeError:  # removed or hidden: keep the link, the user can open it
                c = {}
            rows.append({"name": c.get("name", f"Collection {link['slug']}"), "endorsements": c.get("endorsements"),
                         "url": f"https://www.nexusmods.com/games/{nexus.GAME}/collections/{link['slug']}"})
        else:
            rows.append({"name": link["repo"], "url": f"https://github.com/{link['repo']}"})
    names = nexus.mod_names(ids) if ids else {}
    for r in rows:
        n = names.get(r.pop("mod_id", None), {})
        r.update(name=n.get("name", r["name"]), endorsements=n.get("endorsements", r.get("endorsements")), why="Linked in the video")
    return rows


def phrases(title, text):
    """Word lists worth a Nexus name search: the title, then the chapter titles or else the first lines of the text.
    Each is cut at separators (| - : ! ( ...), and common words (cyberpunk, mod, showcase, the ...) are dropped."""
    out = []
    for piece in [title] + (CHAPTER.findall(text) or text.splitlines()[:5]):
        piece = re.sub(r"https?://\S+|[#@][\w.]+", " ", piece)
        for seg in re.split(r"\s[-–—]\s|[|•:;!?()\[\]{}\"“”/,+&]", piece):
            words = [w.strip("'") for w in re.findall(r"[\w']+", seg)]
            words = [w for w in words if w and w.lower() not in NOISE and not w.isdigit()][:6]
            if any(len(w) >= 3 for w in words) and words not in out:
                out.append(words)
    return out[:6]


def find(entry):
    """(video title, [{"name", "url", "why", "endorsements"}]) for a video link, or for pasted text such as a
    caption or a mod name. Links in the video win; the name search runs only when it links no Nexus mod."""
    entry = entry.strip()
    if len(entry) < 2000 and re.fullmatch(r"(?:www\.|m\.)?[\w-]+(?:\.[\w-]+)*\.[a-z]{2,}/\S*", entry, re.I) \
            and not LINK.match(entry):  # youtu.be/x without https:// (a Nexus or GitHub link is read as it is)
        entry = "https://" + entry
    title, text = video_text(entry) if re.fullmatch(r"https?://\S+", entry) else ("", entry)
    title, text = title[:300], text[:5000]  # YouTube's limit; much more is a page that makes the patterns below slow
    rows = links(f"{title}\n{text}")
    if any("nexusmods" in r["url"] for r in rows):
        return title, rows
    seen = set()
    for words in phrases(title, text):
        for n in nexus.search(words)[:3]:
            if n["modId"] not in seen:
                seen.add(n["modId"])
                rows.append({"name": n["name"], "url": f"https://www.nexusmods.com/{nexus.GAME}/mods/{n['modId']}",
                             "why": f"Name matches \"{' '.join(words)}\"", "endorsements": n["endorsements"]})
    return title, rows
