import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

from installer.install import TAR, _inside, fomod_tree, install, layout, needs_for
from tests.helpers import make_zip

FOMOD = """<?xml version="1.0" encoding="utf-8"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <moduleName>Test</moduleName>
  <requiredInstallFiles><folder source="core" destination="" /></requiredInstallFiles>
  <installSteps><installStep name="Pick"><optionalFileGroups>
    <group name="Colour" type="SelectExactlyOne"><plugins>
      <plugin name="Red"><files><file source="opts\\red.archive" destination="archive\\pc\\mod\\red.archive" /></files>
        <conditionFlags><flag name="col">red</flag></conditionFlags><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
      <plugin name="Blue"><files><file source="opts\\blue.archive" destination="archive\\pc\\mod\\" /></files>
        <conditionFlags><flag name="col">blue</flag></conditionFlags><typeDescriptor><type name="Recommended"/></typeDescriptor></plugin>
    </plugins></group>
    <group name="Extras" type="SelectAny"><plugins>
      <plugin name="Evil"><files><file source="opts\\red.archive" destination="..\\..\\evil.archive" /></files>
        <typeDescriptor><type name="Required"/></typeDescriptor></plugin>
    </plugins></group>
  </optionalFileGroups></installStep></installSteps>
  <conditionalFileInstalls><patterns><pattern>
    <dependencies operator="And"><flagDependency flag="col" value="blue"/></dependencies>
    <files><file source="opts\\blue.yaml" destination="r6\\tweaks\\blue.yaml" /></files>
  </pattern></patterns></conditionalFileInstalls>
</config>"""


class Install(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())

    def run_install(self, files, name="My Mod", archive="m.zip"):
        return install(make_zip(self.tmp / archive, files), self.tmp / "stage", name)

    def test_rooted_inside_wrapper_folder(self):
        rels = self.run_install({"MyMod v2/archive/pc/mod/a.archive": "x", "MyMod v2/r6/scripts/m/a.reds": "x", "MyMod v2/readme.txt": "x"})
        self.assertEqual(rels, ["archive/pc/mod/a.archive", "r6/scripts/m/a.reds"])
        self.assertTrue((self.tmp / "stage/archive/pc/mod/a.archive").exists())

    def test_loose_files(self):
        rels = self.run_install({"a.archive": "x", "a.archive.xl": "x", "src/main.reds": "x", "src/sub/b.reds": "x",
                                 "t.yaml": "x", "keys.xml": "x", "readme.md": "x", "pic.png": "x"})
        self.assertEqual(rels, ["archive/pc/mod/a.archive", "archive/pc/mod/a.archive.xl", "r6/input/keys.xml",
                                "r6/scripts/My Mod/main.reds", "r6/scripts/My Mod/sub/b.reds", "r6/tweaks/My Mod/t.yaml"])

    def test_cet_mod_loose_and_cet_relative(self):
        rels = self.run_install({"Cool/init.lua": "x", "Cool/modules/x/init.lua": "x", "Cool/data.json": "x"})
        self.assertEqual(rels, ["bin/x64/plugins/cyber_engine_tweaks/mods/Cool/data.json",
                                "bin/x64/plugins/cyber_engine_tweaks/mods/Cool/init.lua",
                                "bin/x64/plugins/cyber_engine_tweaks/mods/Cool/modules/x/init.lua"])
        rels = self.run_install({"mods/Cool/init.lua": "x"}, archive="n.zip")
        self.assertEqual(rels, ["bin/x64/plugins/cyber_engine_tweaks/mods/Cool/init.lua"])

    def test_red4ext_plugin_and_redmod(self):
        rels = self.run_install({"Plug/Plug.dll": "x", "Plug/cfg.toml": "x"})
        self.assertEqual(rels, ["red4ext/plugins/Plug/Plug.dll", "red4ext/plugins/Plug/cfg.toml"])
        rels = self.run_install({"mods/Big/info.json": "{}", "mods/Big/archives/b.archive": "x"}, archive="r.zip")
        self.assertEqual(rels, ["mods/Big/archives/b.archive", "mods/Big/info.json"])

    def test_fomod_defaults_flags_and_path_guard(self):
        rels = self.run_install({"fomod/ModuleConfig.xml": FOMOD, "core/r6/scripts/c.reds": "x", "opts/red.archive": "r",
                                 "opts/blue.archive": "b", "opts/blue.yaml": "y"})
        self.assertEqual(rels, ["archive/pc/mod/blue.archive", "r6/scripts/c.reds", "r6/tweaks/blue.yaml"])

    def test_fomod_cannot_write_outside_its_folder(self):
        tree, out = self.tmp / "a/b/tree", self.tmp / "a/b/out"
        for name, body in {"fomod/ModuleConfig.xml": FOMOD, "opts/red.archive": "r", "opts/blue.archive": "b"}.items():
            (tree / name).parent.mkdir(parents=True, exist_ok=True)
            (tree / name).write_text(body)
        fomod_tree(tree, out)
        self.assertFalse((self.tmp / "a/evil.archive").exists())  # "..\..\evil.archive" from out
        self.assertEqual([p.name for p in self.tmp.rglob("evil.archive")], [])

    def test_archive_name_cannot_inject_tar_options(self):
        # A full-width quote becomes a real quote in tar's ANSI command line ("best-fit"); "--version" must stay a name.
        rels = self.run_install({"a.archive": "x"}, archive="mod＂ --version ＂.zip")
        self.assertEqual(rels, ["archive/pc/mod/a.archive"])

    def test_read_only_archive_installs_and_stays_unchanged(self):
        z = make_zip(self.tmp / "ro.zip", {"a.archive": "x"})
        os.chmod(z, stat.S_IREAD)
        self.addCleanup(os.chmod, z, stat.S_IWRITE | stat.S_IREAD)
        self.assertEqual(install(z, self.tmp / "stage", "RO"), ["archive/pc/mod/a.archive"])
        self.assertFalse(os.access(z, os.W_OK))  # still read-only: nothing touched the user's file

    def test_fomod_folder_with_a_junction_is_skipped(self):
        outside = self.tmp / "outside"
        outside.mkdir()
        (outside / "secret.txt").write_text("private")
        tree, out = self.tmp / "tree", self.tmp / "out"
        (tree / "fomod").mkdir(parents=True)
        (tree / "opts").mkdir()
        (tree / "opts" / "ok.reds").write_text("x")
        r = subprocess.run(["cmd", "/c", "mklink", "/J", str(tree / "opts" / "link"), str(outside)], capture_output=True)
        if r.returncode:
            self.skipTest("could not create a junction")
        (tree / "fomod" / "ModuleConfig.xml").write_text(
            '<config><requiredInstallFiles><folder source="opts" destination="r6/scripts/m" /></requiredInstallFiles></config>')
        fomod_tree(tree, out)
        self.assertFalse((out / "r6").exists())  # the whole pick is skipped, so nothing goes through the link
        self.assertEqual(list(out.rglob("secret.txt")), [])

    def test_inside_refuses_every_way_out(self):
        for bad in ("..\\..\\evil.archive", "../x", "\\Users\\Public\\evil.archive", "/Users/Public/evil.archive",
                    "D:evil.dll", "C:/x", "C:\\x", "\\\\server\\share\\x", "a/../../x"):
            self.assertFalse(_inside(bad), bad)
        for ok in ("", "archive\\pc\\mod\\", "archive/pc/mod/a.archive", "Option A", "r6\\scripts"):
            self.assertTrue(_inside(ok), ok)

    def test_fomod_that_picks_nothing_gives_a_clear_error(self):
        empty = FOMOD.replace('type="SelectExactlyOne"', 'type="SelectAny"').replace('"Recommended"', '"Optional"') \
            .replace('<type name="Required"/>', '<type name="Optional"/>').replace('<folder source="core" destination="" />', "")
        with self.assertRaisesRegex(RuntimeError, "no files the game can load"):
            self.run_install({"fomod/ModuleConfig.xml": empty, "core/x.reds": "x"})

    def test_fomod_with_entities_is_ignored(self):
        bomb = FOMOD.replace("<config", '<!DOCTYPE c [<!ENTITY a "aaaa">]><config')
        rels = self.run_install({"fomod/ModuleConfig.xml": bomb, "x.archive": "x"})
        self.assertEqual(rels, ["archive/pc/mod/x.archive"])

    def test_7z_unpacks(self):
        src = self.tmp / "src"
        (src / "archive/pc/mod").mkdir(parents=True)
        (src / "archive/pc/mod/z.archive").write_text("x")
        subprocess.run([str(TAR), "--format", "7zip", "-cf", str(self.tmp / "z.7z"), "-C", str(src), "archive"], check=True)
        self.assertEqual(install(self.tmp / "z.7z", self.tmp / "stage", "Z"), ["archive/pc/mod/z.archive"])

    def test_symlinks_in_archives_are_not_installed(self):
        tree = self.tmp / "tree"
        (tree / "archive/pc/mod").mkdir(parents=True)
        (tree / "archive/pc/mod/ok.archive").write_text("x")
        secret = self.tmp / "secret.txt"
        secret.write_text("private")
        try:
            (tree / "archive/pc/mod/leak.archive").symlink_to(secret)
        except OSError:
            self.skipTest("this Windows account cannot create symlinks")
        self.assertEqual([rel for _, rel in layout(tree, "M")], ["archive/pc/mod/ok.archive"])

    def test_nothing_usable(self):
        with self.assertRaises(RuntimeError):
            self.run_install({"readme.txt": "x"})

    def test_needs(self):
        self.assertEqual(needs_for(["red4ext/plugins/X/x.dll", "red4ext/plugins/X/Scripts/a.reds", "archive/pc/mod/a.xl",
                                    "r6/tweaks/a.yaml", "bin/x64/plugins/cyber_engine_tweaks/mods/m/init.lua", "r6/input/k.xml"]),
                         {"RED4ext", "redscript", "ArchiveXL", "TweakXL", "Cyber Engine Tweaks", "Input Loader"})
        self.assertEqual(needs_for(["archive/pc/mod/a.archive"]), set())


if __name__ == "__main__":
    unittest.main()
