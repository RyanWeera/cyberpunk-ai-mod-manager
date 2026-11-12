"""The tools that AI apps (Claude Desktop, Claude Code, Cursor and other MCP apps) can call. `TOOLS` is what the
MCP server lists; `run` does a call inside the mod manager window, with the same controller the buttons use."""
import json

from downloads import nexus
from downloads.fetch import FW_BY_REPO
from video_finder import finder

TOOLS = [
    {"name": "add_mods", "description": "Download and install Cyberpunk 2077 mods in the user's CP2077 AI Mod Manager. "
     "The manager also installs each mod's required mods and frameworks, virus-scans the files and copies them into the game.",
     "inputSchema": {"type": "object", "required": ["links"], "properties": {"links": {"type": "string", "description":
         "One link per line: Nexus Mods mod pages or collections (cyberpunk2077), or the GitHub repository of a framework "
         "such as RED4ext. For a video link (YouTube, TikTok, Instagram...) call find_mods_in_video first. Other GitHub "
         "repositories, direct download links and files on the PC are not taken from AI apps: the user can paste them in "
         "the manager's Add mods tab."}}}},
    {"name": "find_mods_in_video", "description": "List the Cyberpunk 2077 mods that a video shows, from the links and text in "
     "its description or caption (YouTube works best). Rows found by name only are guesses: confirm them with the user.",
     "inputSchema": {"type": "object", "required": ["url"], "properties": {"url": {"type": "string", "description":
         "The video link, or a caption or mod name to search Nexus for."}}}},
    {"name": "status", "description": "The manager's state: game folder and Nexus account set-up, downloads in progress "
     "(and the files that wait for the user's click on Nexus), and the installed mods in load order.",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "sort_load_order", "description": "Put the installed mods in the best load order (frameworks first, each mod "
     "below the mods it needs, patches below the mods they change). The user can undo it in the Crash doctor tab.",
     "inputSchema": {"type": "object", "properties": {}}},
    {"name": "check_problems", "description": "Look for missing frameworks and required mods, mod errors in the game logs "
     "and file conflicts. Returns each problem with the fixes the user can apply in the Crash doctor tab.",
     "inputSchema": {"type": "object", "properties": {}}},
]


def add_mods(ctl, links):
    ok, bad = [], []
    for line in (ln.strip() for ln in links.splitlines()):
        if not line:
            continue
        # Nexus Mods and framework web links only: a \\host\share path makes Windows log in to that host, and text
        # the AI read (a video description, a mod page) could ask it to add an archive or a GitHub release from anyone.
        try:
            if not line.lower().startswith(("https://", "http://")):
                raise ValueError(f"Not a web link: {line}")
            link = nexus.parse_link(line)
            if not (link["type"] in ("nexus", "collection") or link["type"] == "github" and link["repo"].lower() in FW_BY_REPO):
                raise ValueError(f"Not a Nexus Mods link: {line}. Ask the user to paste it in the Add mods tab")
            ok.append(line)
        except ValueError as e:
            bad.append(f"{e} (for a video link, call find_mods_in_video)")
    if ok:
        ctl.add_links("\n".join(ok))
    out = [f"Added {len(ok)} link(s). The manager downloads and installs them now; call status to follow them."] if ok else []
    if bad:
        out += ["Not added:"] + bad
    if not ctl.store.game():
        out.append("The Cyberpunk 2077 folder is not set: ask the user to set it in the manager's Settings tab.")
    if any("nexusmods" in x.lower() for x in ok):
        if not ctl.key:
            out.append("No Nexus API key is set: ask the user to add it in the Settings tab ('Get my key').")
        elif not ctl.premium:
            out.append("Free Nexus account: the manager opens each file's Nexus page in the browser, and the user must "
                       "click 'Slow download' on each one. Tell the user.")
    return "\n".join(out)


def find_mods_in_video(ctl, url):
    title, rows = finder.find(url)
    if not rows:
        return "No mods found in the video's text. Ask the user for the mod's name."
    head = [f'Video: "{title}"'] if title else []
    return "\n".join(head + [f"- {r['name']}: {r['url']} ({r['why']}" + (f", {r['endorsements']} endorsements)" if r["endorsements"] else ")")
                             for r in rows] + ["Pass the links to add to add_mods. Rows found by name are guesses: ask the user."])


def status(ctl):
    st = ctl.store
    with ctl.lock:
        jobs = [dict(j) for j in ctl.jobs]
    out = [f"Game folder: {st.game() or 'not set (Settings tab)'}",
           f"Nexus: {'no API key (Settings tab)' if not ctl.key else 'Premium' if ctl.premium else 'free account'}"]
    if jobs:
        out.append("Downloads:")
        out += [f"- {j['name']}: {j['status']} ({j['state']})" for j in jobs]
        if any(j["state"] == "waiting" for j in jobs):
            out.append("Waiting files need the user to click 'Slow download' on the Nexus page the manager opened.")
    mods = st.mods
    out.append(f"Installed mods, in load order ({len(mods)}; the last one wins):")
    out += [f"{n}. {m['name']}" + ("" if m["enabled"] else " (off)") for n, m in enumerate(mods, 1)]
    return "\n".join(out)


def sort_load_order(ctl):
    return ctl.auto_sort()


def check_problems(ctl):
    probs = ctl.check_now()
    if not probs:
        return "No problems found."
    return "\n".join([f"- [{p['severity']}] {p['title']}: {str(p['detail'])[:300]}\n  Fixes: " + " / ".join(f["label"] for f in p["fixes"])
                      for p in probs] + ["The user applies fixes in the manager's Crash doctor tab (or with 'Ask the local AI')."])


def run(ctl, msg):
    """One call from the MCP server process, JSON {"tool", "args"} -> {"text"} or {"error"}."""
    try:
        req = json.loads(msg)
        fn = {t["name"]: globals()[t["name"]] for t in TOOLS}.get(req["tool"])
        if not fn:
            return {"error": f"Unknown tool: {req['tool']}"}
        return {"text": fn(ctl, **(req.get("args") or {}))}
    except Exception as e:  # noqa: BLE001 - goes back to the AI app as the tool's error
        return {"error": str(e) or type(e).__name__}
