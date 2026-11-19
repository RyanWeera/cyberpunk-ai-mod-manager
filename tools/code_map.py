"""Rebuild the generated part of CODE_MAP.md: every module with its classes, functions, constants and line numbers.

Run: python tools/code_map.py   (after any code change, so the line numbers stay right)
"""
import ast
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MAP = ROOT / "CODE_MAP.md"
BEGIN, END = "<!-- BEGIN GENERATED: python tools/code_map.py -->", "<!-- END GENERATED -->"
SKIP = {"__pycache__", "graft", ".git", ".venv", "build", "dist", "model", "engine", "tools"}


def modules():
    return sorted(p for p in ROOT.rglob("*.py") if not SKIP & set(p.relative_to(ROOT).parts) and p.stat().st_size)


def summary(node):
    """First sentence of a docstring on one line, at most 150 characters."""
    d = ast.get_docstring(node) or ""
    d = " ".join(d.split("\n\n")[0].split())
    d = re.split(r"(?<!e\.g)(?<!i\.e)(?<=[.!?])\s", d, maxsplit=1)[0]
    return d if len(d) <= 150 else d[:147] + "..."


def imports(tree, known):
    """Project modules (as paths in `known`) that this module imports."""
    found = set()
    for n in ast.walk(tree):
        if isinstance(n, ast.Import):
            found |= {a.name.replace(".", "/") + ".py" for a in n.names}
        elif isinstance(n, ast.ImportFrom) and n.module:
            base = n.module.replace(".", "/")
            found |= {f"{base}/{a.name}.py" for a in n.names} | {base + ".py"}
    return found & known


def render():
    mods = modules()
    rel = {p: p.relative_to(ROOT).as_posix() for p in mods}
    trees = {p: ast.parse(p.read_text("utf-8")) for p in mods}
    uses = {p: sorted(imports(trees[p], set(rel.values())) - {rel[p]}) for p in mods}
    out = []
    for p in mods:
        tree, lines = trees[p], p.read_text("utf-8").count("\n") + 1
        used_by = sorted(rel[q] for q in mods if rel[p] in uses[q] and not rel[q].startswith("tests/"))
        out += ["", f"### `{rel[p]}` ({lines} lines)", summary(tree) or "-"]
        if uses[p]:
            out.append(f"Uses: {', '.join(uses[p])}")
        if used_by and not rel[p].startswith("tests/"):
            out.append(f"Used by: {', '.join(used_by)}")
        out.append("")
        for n in tree.body:
            if isinstance(n, ast.Assign) and any(isinstance(t, ast.Name) and t.id.isupper() for t in n.targets):
                out.append(f"- L{n.lineno} `{', '.join(t.id for t in n.targets if isinstance(t, ast.Name))}` (constant)")
            elif isinstance(n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                kind = "class" if isinstance(n, ast.ClassDef) else "def"
                s = summary(n)
                out.append(f"- L{n.lineno} {kind} `{n.name}`" + (f": {s}" if s else ""))
                if kind == "class" and not rel[p].startswith("tests/"):
                    for m in n.body:
                        if isinstance(m, (ast.FunctionDef, ast.AsyncFunctionDef)):
                            s = summary(m)
                            out.append(f"  - L{m.lineno} `.{m.name}`" + (f": {s}" if s else ""))
    return "\n".join(out).strip("\n")


def main():
    text = MAP.read_text("utf-8")
    head, _, rest = text.partition(BEGIN)
    _, sep, tail = rest.partition(END)
    if not sep:
        sys.exit(f"{MAP.name} lacks the {BEGIN} ... {END} markers")
    MAP.write_text(f"{head}{BEGIN}\n{render()}\n{END}{tail}", "utf-8")
    print(f"Updated {MAP.name}: {len(modules())} modules")


if __name__ == "__main__":
    main()
