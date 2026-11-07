import tempfile
import unittest
from pathlib import Path

from installer.deploy import deploy
from shared.store import Store
from tests.helpers import fake_game, mod, stage


class Deploy(unittest.TestCase):
    def setUp(self):
        tmp = Path(tempfile.mkdtemp())
        self.game = fake_game(tmp)
        self.store = Store(tmp / "data")
        self.store.settings["game_dir"] = str(self.game)

    def test_priority_disable_and_backup(self):
        (self.game / "bin/x64/global.ini").write_text("vanilla")
        stage(self.store, mod("a", ["archive/pc/mod/a.archive", "bin/x64/global.ini", "r6/scripts/a/x.reds"]))
        stage(self.store, mod("b", ["archive/pc/mod/b.archive", "bin/x64/global.ini"]))
        self.assertEqual(deploy(self.store), 4)  # 5 file entries, 4 distinct paths
        self.assertEqual(deploy(self.store), 0)  # nothing changed -> nothing copied
        self.assertEqual((self.game / "bin/x64/global.ini").read_text(), "b:bin/x64/global.ini")  # lower in list wins
        self.assertEqual((self.game / "archive/pc/mod/modlist.txt").read_text().split(), ["b.archive", "a.archive"])

        self.store.mod("b")["enabled"] = False
        deploy(self.store)
        self.assertEqual((self.game / "bin/x64/global.ini").read_text(), "a:bin/x64/global.ini")
        self.assertFalse((self.game / "archive/pc/mod/b.archive").exists())

        self.store.mod("a")["enabled"] = False
        deploy(self.store)
        self.assertEqual((self.game / "bin/x64/global.ini").read_text(), "vanilla")  # backup restored
        self.assertFalse((self.game / "r6/scripts/a").exists())  # empty mod folder pruned
        self.assertEqual(self.store.deployed, {})

    def test_edited_files_are_kept_until_the_mod_changes(self):
        stage(self.store, mod("a", ["r6/input/keys.xml"]))
        deploy(self.store)
        (self.game / "r6/input/keys.xml").write_text("my own keys")
        self.assertEqual(deploy(self.store), 0)
        self.assertEqual((self.game / "r6/input/keys.xml").read_text(), "my own keys")
        self.store.deployed["r6/input/keys.xml"] = None  # what an update of mod "a" does
        deploy(self.store)
        self.assertEqual((self.game / "r6/input/keys.xml").read_text(), "a:r6/input/keys.xml")

    def test_new_game_folder_takes_mods_out_of_the_old_one(self):
        (self.game / "bin/x64/global.ini").write_text("vanilla A")
        stage(self.store, mod("a", ["bin/x64/global.ini", "archive/pc/mod/a.archive"]))
        deploy(self.store)
        other = fake_game(Path(tempfile.mkdtemp()))
        (other / "bin/x64/global.ini").write_text("vanilla B")
        self.store.settings["game_dir"] = str(other)
        deploy(self.store)
        self.assertEqual((self.game / "bin/x64/global.ini").read_text(), "vanilla A")  # old folder restored
        self.assertFalse((self.game / "archive/pc/mod/a.archive").exists())
        self.assertEqual((other / "bin/x64/global.ini").read_text(), "a:bin/x64/global.ini")
        self.store.mod("a")["enabled"] = False
        deploy(self.store)
        self.assertEqual((other / "bin/x64/global.ini").read_text(), "vanilla B")  # B's own backup, not A's

    def test_state_is_saved_when_a_copy_fails(self):
        stage(self.store, mod("a", ["archive/pc/mod/a.archive", "archive/pc/mod/b.archive"]))
        (self.store.staging / "a/archive/pc/mod/b.archive").unlink()  # the second copy will fail
        with self.assertRaises(OSError):
            deploy(self.store)
        saved = Store(self.store.root)
        self.assertEqual(saved.deployed, {"archive/pc/mod/a.archive": "a"})

    def test_unmanaged_archives_kept_last(self):
        (self.game / "archive/pc/mod/zzz_manual.archive").write_text("x")
        stage(self.store, mod("a", ["archive/pc/mod/a.archive"]))
        deploy(self.store)
        self.assertEqual((self.game / "archive/pc/mod/modlist.txt").read_text().split(), ["a.archive", "zzz_manual.archive"])

    def test_needs_game_folder(self):
        self.store.settings["game_dir"] = ""
        with self.assertRaises(RuntimeError):
            deploy(self.store)


if __name__ == "__main__":
    unittest.main()
