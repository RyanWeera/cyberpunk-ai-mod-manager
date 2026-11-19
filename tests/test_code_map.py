"""tools/code_map.py: the generated index of CODE_MAP.md."""
import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("code_map", ROOT / "tools" / "code_map.py")
code_map = importlib.util.module_from_spec(spec)
spec.loader.exec_module(code_map)


class Index(unittest.TestCase):
    def test_every_module_is_listed(self):
        text = code_map.render()
        for p in code_map.modules():
            self.assertIn(f"### `{p.relative_to(ROOT).as_posix()}`", text)

    def test_line_numbers_point_at_the_definition(self):
        text = code_map.render()
        self.assertIn("Used by: manager_ui/controller.py", text)
        src = (ROOT / "installer" / "install.py").read_text("utf-8").splitlines()
        line = next(x for x in text.splitlines() if "def `install`" in x)
        self.assertTrue(src[int(line.split()[1][1:]) - 1].startswith("def install("))


if __name__ == "__main__":
    unittest.main()
