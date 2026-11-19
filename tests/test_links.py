import unittest

from downloads.fetch import ascii_name
from unittest import mock

from downloads import nexus
from downloads.nexus import files_page, newer_file, outdated, parse_link, pick_file


class FileNames(unittest.TestCase):
    def test_names_are_plain_ascii(self):
        self.assertEqual(ascii_name("mod＂ --version ＂.zip"), "mod_ --version _.zip")
        self.assertTrue(ascii_name("Ärger Mod v1.2 (fix).7z").isascii())
        self.assertEqual(ascii_name("..\\..\\x.zip"), "_.._x.zip")
        self.assertEqual(ascii_name("..."), "download.zip")


class Outdated(unittest.TestCase):
    def test_reasons_a_mod_may_not_work(self):
        self.assertEqual(outdated("Better Cars", "published", "2025-01-01T00:00:00Z"), "")
        self.assertIn("removed or hid", outdated("Better Cars", "hidden", "2025-01-01"))
        self.assertIn("name says", outdated("Better Cars [OUTDATED]", "published", "2025-01-01"))
        self.assertEqual(outdated("Fix for broken headlights", "published", "2025-01-01"), "")  # a fix, not a broken mod
        self.assertIn("2.0", outdated("Better Cars", "published", "2022-05-07T10:00:00Z"))


class GithubCopy(unittest.TestCase):
    def test_only_the_same_version_of_the_mods_own_repo(self):
        desc = ("Needs [url=https://github.com/psiberx/cp2077-codeware]Codeware[/url]. Source: https://github.com/psiberx/"
                "cp2077-equipment-ex and https://github.com/someone/other-tool and github.com/../equipment-x")
        gql = lambda q, api_key="", **v: {"mod": {"name": "Equipment-EX", "description": desc}}
        u = "https://github.com/psiberx/cp2077-equipment-ex/releases/download/v1.3.1/EquipmentEx-1.3.1.zip"
        rel = {"tag_name": "v1.3.1", "assets": [{"name": "EquipmentEx-1.3.1.zip", "browser_download_url": u},
                                                {"name": "EquipmentEx.pdb.zip", "browser_download_url": "p"}]}
        with mock.patch.object(nexus, "gql", gql), mock.patch.object(nexus, "http_json", return_value=rel) as get:
            self.assertEqual(nexus.github_copy(6945, "1.3.1"), {"repo": "psiberx/cp2077-equipment-ex", "url": u})
            get.assert_called_once_with("https://api.github.com/repos/psiberx/cp2077-equipment-ex/releases/latest")  # not "../"
            self.assertIsNone(nexus.github_copy(6945, "1.3.0"))  # another version: the Nexus file is the one asked for
            rel["assets"][0]["browser_download_url"] = "https://example.com/EquipmentEx-1.3.1.zip"
            self.assertIsNone(nexus.github_copy(6945, "1.3.1"))  # a file that is not on GitHub
        rel["assets"].append({"name": "EquipmentEx-lite.zip", "browser_download_url": "l"})
        with mock.patch.object(nexus, "gql", gql), mock.patch.object(nexus, "http_json", return_value=rel):
            self.assertIsNone(nexus.github_copy(6945, "1.3.1"))  # two variants: no guessing


class Links(unittest.TestCase):
    def test_nexus_mod_page(self):
        self.assertEqual(parse_link("https://www.nexusmods.com/cyberpunk2077/mods/107"), {"type": "nexus", "mod_id": 107, "file_id": None})
        self.assertEqual(parse_link("https://www.nexusmods.com/cyberpunk2077/mods/107?tab=files&file_id=999")["file_id"], 999)

    def test_collections(self):
        want = {"type": "collection", "slug": "iszwwe", "revision": None}
        self.assertEqual(parse_link("https://www.nexusmods.com/games/cyberpunk2077/collections/iszwwe"), want)
        self.assertEqual(parse_link("https://next.nexusmods.com/cyberpunk2077/collections/iszwwe"), want)
        self.assertEqual(parse_link("https://www.nexusmods.com/cyberpunk2077/collections/iszwwe/revisions/484")["revision"], 484)
        self.assertEqual(parse_link("nxm://cyberpunk2077/collections/iszwwe/revisions/12")["revision"], 12)

    def test_nxm_file(self):
        link = parse_link("nxm://cyberpunk2077/mods/4198/files/1234?key=abc&expires=1700000000&user_id=5")
        self.assertEqual(link, {"type": "nexus", "mod_id": 4198, "file_id": 1234, "key": "abc", "expires": "1700000000"})

    def test_other_sources(self):
        self.assertEqual(parse_link("https://github.com/psiberx/cp2077-codeware/releases"), {"type": "github", "repo": "psiberx/cp2077-codeware"})
        self.assertEqual(parse_link("https://github.com/a/b/releases/download/v1/b.zip")["type"], "url")
        self.assertEqual(parse_link("https://example.com/x.7z")["type"], "url")
        self.assertIsNone(parse_link("   "))
        self.assertIsNone(parse_link("# comment"))

    def test_rejects(self):
        for bad in ("https://www.nexusmods.com/skyrimspecialedition/mods/1", "nxm://skyrim/mods/1/files/2", "hello",
                    "http://example.com/x.zip",  # plain http could be changed on the way
                    "https://github.com/a/b\\..\\..\\x", "https://github.com/a/..", "https://github.com/../x",  # plain names only
                    "https://evilnexusmods.com/cyberpunk2077/mods/1"):  # not a Nexus host
            with self.assertRaises(ValueError):
                parse_link(bad)

    def test_file_choice(self):
        files = [{"fileId": 1, "name": "Main", "category": "OLD_VERSION", "primary": 0, "date": 1},
                 {"fileId": 2, "name": "Main", "category": "MAIN", "primary": 1, "date": 5},
                 {"fileId": 3, "name": "Patch", "category": "OPTIONAL", "primary": 0, "date": 9},
                 {"fileId": 4, "name": "Alt", "category": "MAIN", "primary": 0, "date": 7}]
        self.assertEqual(pick_file(files)["fileId"], 2)
        self.assertEqual(pick_file(files, 3)["fileId"], 3)
        self.assertEqual(newer_file(files, 1)["fileId"], 2)
        self.assertIsNone(newer_file(files, 2))
        self.assertIn("file_id=5&nmm=1", files_page(1, 5))


if __name__ == "__main__":
    unittest.main()


class Browse(unittest.TestCase):
    def test_query_filters_and_result(self):
        from unittest import mock

        from downloads import nexus
        node = {"modId": "7", "name": "Nova", "summary": None, "author": "x", "downloads": 3, "endorsements": 1,
                "updatedAt": "2026-01-01T00:00:00Z", "thumbnailUrl": None, "modCategory": {"name": "Visuals"}}
        page = {"mods": {"totalCount": 41, "facetsData": {"categoryName": {"Visuals": 9}}, "nodes": [node]}}
        with mock.patch.object(nexus, "gql", return_value=page) as gql:
            r = nexus.browse("nova lut", ["Visuals", "Audio"], "endorsements", page=2)
            v = gql.call_args.kwargs
            self.assertEqual(v["o"], 40)
            self.assertEqual([w["value"] for w in v["f"]["filter"][0]["nameStemmed"]], ["nova", "lut"])
            self.assertEqual(v["f"]["adultContent"], [{"value": False, "op": "EQUALS"}])
            self.assertEqual([c["value"] for c in v["p"]["categoryName"]], ["Visuals", "Audio"])
            self.assertEqual(r["mods"][0], {"id": 7, "name": "Nova", "summary": "", "author": "x", "downloads": 3, "endorsements": 1,
                                            "updated": "2026-01-01T00:00:00Z", "thumb": "", "category": "Visuals"})
            self.assertEqual((r["total"], r["categories"]), (41, {"Visuals": 9}))
            nexus.browse(sort="relevance", adult=True)  # no words: best match means nothing, so most downloaded
            v = gql.call_args.kwargs
            self.assertEqual(v["s"], [{"downloads": {"direction": "DESC"}}])
            self.assertNotIn("adultContent", v["f"])
            self.assertEqual(v["p"]["gameDomainName"][0]["value"], "cyberpunk2077")

    def test_numbers_and_dates(self):
        from datetime import UTC, datetime

        from manager_ui.ui import ago, short
        self.assertEqual([short(n) for n in (999, 1000, 248536, 20381596, 1.5e9)], ["999", "1K", "248.5K", "20.4M", "1.5B"])
        now = datetime(2026, 11, 20, tzinfo=UTC)
        self.assertEqual([ago(d, now) for d in ("2026-11-20T01:00:00Z", "2026-11-19T00:00:00Z", "2026-11-10T00:00:00Z",
                                                 "2026-08-01T00:00:00Z", "2023-01-01T00:00:00Z", "bad")],
                         ["today", "1 day ago", "10 days ago", "3 months ago", "3 years ago", ""])
