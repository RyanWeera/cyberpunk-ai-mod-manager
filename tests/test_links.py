import unittest

from downloads.fetch import ascii_name
from downloads.nexus import files_page, newer_file, parse_link, pick_file


class FileNames(unittest.TestCase):
    def test_names_are_plain_ascii(self):
        self.assertEqual(ascii_name("mod＂ --version ＂.zip"), "mod_ --version _.zip")
        self.assertTrue(ascii_name("Ärger Mod v1.2 (fix).7z").isascii())
        self.assertEqual(ascii_name("..\\..\\x.zip"), "_.._x.zip")
        self.assertEqual(ascii_name("..."), "download.zip")


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
