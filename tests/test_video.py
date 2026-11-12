"""Find mod from video, offline: pages and Nexus answers are canned."""
import json
import unittest
import urllib.request
from types import SimpleNamespace
from unittest import mock

from downloads import nexus
from manager_ui.controller import Controller
from video_finder import finder

NAMES = {4714: {"modId": 4714, "name": "Stand Still Please", "endorsements": 6602}}
FOUND = [{"modId": 2, "name": "Nova LUT 4.0 - Russian", "endorsements": 10},
         {"modId": 3, "name": "Nova City Lights", "endorsements": 900},
         {"modId": 1, "name": "Nova LUT 4.0", "endorsements": 5000}]


def fake_gql(query, api_key="", **v):
    if "legacyModsByDomain" in query:
        return {"legacyModsByDomain": {"nodes": [NAMES[i["modId"]] for i in v["ids"] if i["modId"] in NAMES]}}
    if "collection(slug" in query:
        if v["s"] != "khbuxr":
            raise RuntimeError("Collection not found")
        return {"collection": {"name": "Spectr Collection", "endorsements": 11}}
    return {"mods": {"nodes": FOUND}}


def youtube(details):
    return "<script>var ytInitialPlayerResponse = " + json.dumps({"videoDetails": details}) + ";var meta = {};</script>"


class Finder(unittest.TestCase):
    def setUp(self):
        self.pages, self.asked = {}, []
        patches = [mock.patch.object(finder, "page", self.page), mock.patch.object(nexus, "gql", side_effect=fake_gql)]
        self.gql = patches[1].start()
        patches[0].start()
        for p in patches:
            self.addCleanup(p.stop)

    def page(self, url, limit=0):
        self.asked.append(url)
        return self.pages[url]

    def test_youtube_ids(self):
        for url in ("https://youtu.be/8Yexbiw0T28?si=x", "https://www.youtube.com/watch?v=8Yexbiw0T28&t=5",
                    "https://m.youtube.com/shorts/8Yexbiw0T28", "https://youtube.com/live/8Yexbiw0T28"):
            self.assertEqual(finder.youtube_id(url), "8Yexbiw0T28", url)
        self.assertIsNone(finder.youtube_id("https://www.tiktok.com/@a/video/1"))

    def test_youtube_links_win(self):
        self.pages["https://www.youtube.com/watch?v=8Yexbiw0T28"] = (youtube({"title": "Top mods", "shortDescription": (
            "0:00 Intro\n1:23 Better Vehicle Handling\nMods:\nhttps://www.nexusmods.com/cyberpunk2077/mods/4714?tab=files\n"
            "nexusmods.com/skyrim/mods/1\nhttps://github.com/psiberx/cp2077-codeware/releases\n"
            "https://www.nexusmods.com/cyberpunk2077/mods/4714.\nhttps://next.nexusmods.com/cyberpunk2077/collections/khbuxr\n"
            "https://www.nexusmods.com/games/cyberpunk2077/collections/gone11")}), "")
        title, rows = finder.find("https://youtu.be/8Yexbiw0T28")
        self.assertEqual(title, "Top mods")
        why = "Linked in the video"
        self.assertEqual(rows, [
            {"name": "Stand Still Please", "url": "https://www.nexusmods.com/cyberpunk2077/mods/4714", "endorsements": 6602, "why": why},
            {"name": "psiberx/cp2077-codeware", "url": "https://github.com/psiberx/cp2077-codeware", "endorsements": None, "why": why},
            {"name": "Spectr Collection", "url": "https://www.nexusmods.com/games/cyberpunk2077/collections/khbuxr", "endorsements": 11,
             "why": why},
            {"name": "Collection gone11", "url": "https://www.nexusmods.com/games/cyberpunk2077/collections/gone11",
             "endorsements": None, "why": why}])
        self.assertEqual(self.gql.call_count, 3)  # names + 2 collections; a linked Nexus mod means no name search

    def test_name_search_when_nothing_is_linked(self):
        title, rows = finder.find("Nova LUT 4.0 | best reshade")
        self.assertEqual(title, "")
        self.assertEqual([r["name"] for r in rows], ["Nova LUT 4.0", "Nova LUT 4.0 - Russian", "Nova City Lights"])
        self.assertEqual(rows[0]["why"], 'Name matches "Nova LUT"')
        self.assertEqual(rows[0]["url"], "https://www.nexusmods.com/cyberpunk2077/mods/1")

    def test_phrases(self):
        self.assertEqual(finder.phrases("Cyberpunk 2077 - Realistic Driving Mod | 4K", "0:00 Intro\n2:10 Nova LUT (night)"),
                         [["Realistic", "Driving"], ["Nova", "LUT"], ["night"]])
        self.assertEqual(finder.phrases("", "#cyberpunk @someone https://x.y/z\nBlackwall Unleashed!"),
                         [["Blackwall", "Unleashed"]])

    def test_tiktok_short_link(self):
        vid = "7590235962194627856"
        self.pages["https://vm.tiktok.com/ZM123/"] = ("", f"https://www.tiktok.com/@u/video/{vid}?lang=en")
        state = {"source": {"data": {f"/embed/{vid}": {"videoData": {"itemInfos": {"text": "Native Unleash Blackwall #cp2077"}}}}}}
        self.pages[f"https://www.tiktok.com/embed/{vid}"] = (
            f'<script id="__FRONTITY_CONNECT_STATE__" type="application/json">{json.dumps(state)}</script>', "")
        self.assertEqual(finder.video_text("https://vm.tiktok.com/ZM123/"), ("", "Native Unleash Blackwall #cp2077"))

    def test_instagram_caption(self):
        self.pages["https://www.instagram.com/p/AB_c1/embed/captioned/"] = (
            '<div class="Caption"><a class="CaptionUsername" href="/u">someone</a><br />Mod: Better Lights &amp; more'
            '<br/><a href="/t">#cyberpunk</a></div><div class="CaptionComments">', "")
        self.assertEqual(finder.video_text("https://www.instagram.com/reel/AB_c1/?igsh=x"), ("", "Mod: Better Lights & more\n#cyberpunk"))

    def test_other_site_meta(self):
        self.pages["https://x.com/a/status/1"] = (
            '<title>X</title><meta property="og:title" content="Better &amp; Brighter"><meta name="description" content="A mod">', "")
        self.assertEqual(finder.video_text("https://x.com/a/status/1"), ("Better & Brighter", "A mod"))

    def test_hostile_page_stays_fast_and_links_need_no_https(self):
        self.pages["https://x.com/long"] = ("<meta " + "a" * 200_000 + '><meta property="og:title" content="T">', "")
        self.assertEqual(finder.video_text("https://x.com/long"), ("T", ""))  # minutes with a slow pattern
        with mock.patch.object(finder, "video_text", return_value=("", "a." * 100_000)) as vt:
            self.assertEqual(finder.find("youtu.be/dQw4w9WgXcQ")[1], [])
            self.assertEqual(finder.find("www.nexusmods.com/cyberpunk2077/mods/4714")[1][0]["name"], "Stand Still Please")
        vt.assert_called_once_with("https://youtu.be/dQw4w9WgXcQ")  # a Nexus link is read as it is, not opened as a video

    def test_hostile_instagram_page_stays_fast(self):
        url = "https://www.instagram.com/p/x/embed/captioned/"
        self.pages[url] = ('<div class="Caption">' * 100_000, "")  # minutes with a pattern over the whole page
        self.assertEqual(finder.video_text("https://www.instagram.com/p/x/"), ("", ""))
        self.pages[url] = ('<div class="Caption">' + "<" * 1_000_000 + '<div class="CaptionComments">', "")
        self.assertEqual(len(finder.video_text("https://www.instagram.com/p/x/")[1]), 20_000)

    def test_errors_reach_the_window(self):
        self.pages["https://www.youtube.com/watch?v=8Yexbiw0T28"] = ("<html>consent page</html>", "")
        events = []
        Controller.find_in_video(SimpleNamespace(notify=lambda k, p: events.append((k, p))), "https://youtu.be/8Yexbiw0T28")
        self.assertEqual(events[0][0], "video")
        self.assertIn("YouTube did not show this video", events[0][1]["error"])


def resolves_to(addr):
    return mock.patch("socket.getaddrinfo", return_value=[(2, 1, 6, "", (addr, 0))])


class PublicPagesOnly(unittest.TestCase):
    def test_this_pc_and_the_local_network_are_refused(self):
        for addr in ("127.0.0.1", "10.1.2.3", "192.168.0.10", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1",
                     "::ffff:127.0.0.1", "fe80::1%3"):
            with resolves_to(addr), mock.patch.object(finder.OPENER, "open") as get:
                with self.assertRaisesRegex(ValueError, "local network", msg=addr):
                    finder.page("https://video.example/v")
                get.assert_not_called()
        for bad in ("file:///C:/x", "ftp://video.example/x", "https:///x"):
            with self.assertRaisesRegex(ValueError, "http"):
                finder.public(bad)

    def test_public_pages_load_and_redirects_are_checked(self):
        with resolves_to("93.184.215.14"), mock.patch.object(finder.OPENER, "open") as get:
            get.return_value.__enter__.return_value = SimpleNamespace(read=lambda n: b"hi", url="https://video.example/v")
            self.assertEqual(finder.page("https://video.example/v"), ("hi", "https://video.example/v"))
        redirect, req = finder._PublicRedirects(), urllib.request.Request("https://video.example/v")
        with resolves_to("127.0.0.1"), self.assertRaisesRegex(ValueError, "local network"):
            redirect.redirect_request(req, None, 302, "Found", {}, "http://localhost:8080/admin")
        with self.assertRaisesRegex(ValueError, "http"):
            redirect.redirect_request(req, None, 302, "Found", {}, "ftp://video.example/x")
        with resolves_to("93.184.215.14"):
            self.assertEqual(redirect.redirect_request(req, None, 302, "Found", {}, "https://www.video.example/v").full_url,
                             "https://www.video.example/v")


if __name__ == "__main__":
    unittest.main()
