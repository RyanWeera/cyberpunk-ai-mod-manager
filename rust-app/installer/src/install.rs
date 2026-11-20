//! Turn a downloaded archive into game-relative files: unpack, run a FOMOD installer with its
//! default choices, and put loose files in the folder the game (or its frameworks) loads them from.
use regex::Regex;
use roxmltree::Node;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Windows 10+ ships bsdtar, which unpacks zip, 7z and rar.
pub static TAR: LazyLock<PathBuf> = LazyLock::new(|| shared::sys32().join("tar.exe"));
const NO_WINDOW: u32 = 0x0800_0000;
pub const ROOTS: [&str; 6] = ["archive", "bin", "engine", "r6", "red4ext", "mods"];
pub const CET_MODS: &str = "bin/x64/plugins/cyber_engine_tweaks/mods";

/// Unpack `archive` into the new folder `dest` with tar.exe. `name` is the mod's name for error messages.
/// tar reads its command line in the ANSI code page, where "best-fit" turns some Unicode letters (a full-width
/// quote, for one) into real quotes and splits the arguments. So no name reaches tar's command line: it reads
/// the archive from standard input and unpacks into the fixed name of `dest`, with its parent as working folder
/// (Windows passes that folder separately, so user names with non-English letters are safe too).
pub fn extract(archive: &Path, dest: &Path, name: &str) -> Result<(), String> {
    let f = fs::File::open(archive).map_err(|e| err(e, archive))?;
    let parent = dest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut child = Command::new(&*TAR)
        .args(["-xf".as_ref(), "-".as_ref(), "-C".as_ref(), dest.file_name().unwrap_or_default()])
        .stdin(f)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .current_dir(parent)
        .creation_flags(NO_WINDOW)
        .spawn()
        .map_err(|e| err(e, &TAR))?;
    let mut pipe = child.stderr.take().expect("stderr is piped");
    let reader = std::thread::spawn(move || {
        let mut b = vec![];
        let _ = pipe.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break s;
        }
        if start.elapsed() > Duration::from_secs(1800) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Unpacking {name} took more than 30 minutes"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        let stderr = String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned();
        let text = stderr.trim();
        let tail: String = text.chars().skip(text.chars().count().saturating_sub(300)).collect();
        let ext = suffix(&name_of(archive));
        let hint = if old_windows() && (ext == ".7z" || ext == ".rar") {
            " The tar.exe in Windows 10 cannot open some .7z and .rar files: update to Windows 11, or unpack it \
             yourself, zip it, and add the .zip."
        } else {
            ""
        };
        return Err(format!("Could not unpack {name}: {tail}{hint}"));
    }
    Ok(())
}

/// Windows 10 (build below 22000).
fn old_windows() -> bool {
    shared::win::reg_get(shared::win::HKLM, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "CurrentBuildNumber")
        .and_then(|b| b.trim().parse::<u32>().ok())
        .is_some_and(|b| b < 22000)
}

pub fn safe(name: &str) -> String {
    static BAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\w .-]").expect("regex"));
    let s = BAD.replace_all(name, "");
    let s = s.trim_matches([' ', '.']);
    if s.is_empty() { "mod".into() } else { s.into() }
}

/// A relative path that stays inside its folder. Refuses `..`, and any root or drive: on Windows
/// `\x`, `/x` and `D:x` are not "absolute", but joining them still leaves the folder.
pub fn inside(path: &str) -> bool {
    let rooted = path.starts_with(['/', '\\']);
    let drive = path.chars().nth(1) == Some(':');
    !rooted && !drive && !path.split(['/', '\\']).any(|p| p == "..")
}

/// If the mod has a FOMOD installer, copy the default choices into `out` and return it, else None.
/// Defaults: Required/Recommended options, else the first option of a pick-one group.
pub fn fomod_tree(tree: &Path, out: &Path) -> Result<Option<PathBuf>, String> {
    let cfg = walk(tree).into_iter().find(|p| {
        name_of(p).to_lowercase() == "moduleconfig.xml" && p.parent().is_some_and(|d| name_of(d).to_lowercase() == "fomod")
    });
    let Some(cfg) = cfg else { return Ok(None) };
    let raw = fs::read(&cfg).map_err(|e| err(e, &cfg))?;
    let le: Vec<u8> = "<!ENTITY".encode_utf16().flat_map(u16::to_le_bytes).collect();
    let text = decode(&raw);
    if contains(&raw, b"<!ENTITY") || contains(&raw, &le) || text.contains("<!ENTITY") {
        return Ok(None); // an installer using XML entities (never needed, only used for attacks)
    }
    let opts = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let doc = roxmltree::Document::parse_with_options(&text, opts).map_err(|e| e.to_string())?;
    let (base, root) = (cfg.parent().and_then(Path::parent).unwrap_or(tree), doc.root_element());
    let mut picks: Vec<(String, String, bool)> = vec![]; // (source, destination, is a folder)
    let mut flags: HashMap<Option<String>, String> = HashMap::new();

    take(first(root, "requiredInstallFiles"), &mut picks);
    for group in named(root, "group") {
        let plugins: Vec<Node> = named(group, "plugin").filter(|p| kind(*p) != "NotUsable").collect();
        let gtype = group.attribute("type").unwrap_or("SelectAny");
        let mut chosen: Vec<Node> = if gtype == "SelectAll" {
            plugins.clone()
        } else {
            plugins.iter().copied().filter(|p| matches!(kind(*p), "Required" | "Recommended")).collect()
        };
        if chosen.is_empty() && matches!(gtype, "SelectExactlyOne" | "SelectAtLeastOne") {
            chosen = plugins.iter().take(1).copied().collect();
        }
        if matches!(gtype, "SelectExactlyOne" | "SelectAtMostOne") {
            chosen.truncate(1);
        }
        for p in chosen {
            take(first(p, "files"), &mut picks);
            for fl in named(p, "flag") {
                flags.insert(fl.attribute("name").map(String::from), fl.text().unwrap_or("").trim().to_string());
            }
        }
    }
    for pat in path_all(root, "conditionalFileInstalls/patterns/pattern") {
        let deps = first(pat, "dependencies");
        let conds: Vec<bool> = deps
            .map(|d| {
                named(d, "flagDependency")
                    .map(|f| flags.get(&f.attribute("flag").map(String::from)).map(String::as_str) == Some(f.attribute("value").unwrap_or("")))
                    .collect()
            })
            .unwrap_or_default();
        let or = deps.is_some_and(|d| d.attribute("operator") == Some("Or"));
        let met = if or { conds.iter().any(|c| *c) } else { conds.iter().all(|c| *c) };
        if met {
            take(first(pat, "files"), &mut picks);
        }
    }

    fs::create_dir_all(out).map_err(|e| err(e, out))?; // exists even when the default choices pick no file
    let (base_r, out_r) = (resolve(base), resolve(out));
    for (src, dst, folder) in picks {
        if !(inside(&src) && inside(&dst)) {
            continue; // never let an installer read or write outside the mod folders
        }
        let (s, d) = (join(base, &src), join(out, &dst));
        if !(resolve(&s).starts_with(&base_r) && resolve(&d).starts_with(&out_r)) {
            continue; // after following any link: still inside?
        }
        if folder && s.is_dir() {
            if std::iter::once(s.clone()).chain(walk(&s)).any(|x| is_link(&x)) {
                continue; // a link in the copy could let a later pick write through it, anywhere on the PC
            }
            copy_tree(&s, &d)?;
        } else if s.is_file() && !is_link(&s) {
            let target = if dst.is_empty() || dst.ends_with(['/', '\\']) || d.is_dir() { d.join(name_of(&s)) } else { d };
            if resolve(&target).starts_with(&out_r) {
                copy_to(&s, &target)?;
            }
        }
    }
    Ok(Some(out.to_path_buf()))
}

/// The XML text: UTF-16 with or without a byte order mark, else UTF-8.
fn decode(raw: &[u8]) -> String {
    let utf16 = |b: &[u8], le: bool| {
        let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| if le { u16::from_le_bytes(*c) } else { u16::from_be_bytes(*c) }).collect();
        String::from_utf16_lossy(&units)
    };
    match raw {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [b'<', 0, ..] => utf16(raw, true),
        [0, b'<', ..] => utf16(raw, false),
        _ => String::from_utf8_lossy(raw).into_owned(),
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The element and its descendants with this tag (namespaces ignored), in document order.
fn named<'a, 'i>(n: Node<'a, 'i>, tag: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.descendants().filter(move |x| x.is_element() && x.tag_name().name() == tag)
}

/// The elements at a child path like "a/b".
fn path_all<'a, 'i>(n: Node<'a, 'i>, path: &str) -> Vec<Node<'a, 'i>> {
    let mut cur = vec![n];
    for seg in path.split('/') {
        cur = cur.iter().flat_map(|c| c.children().filter(|x| x.is_element() && x.tag_name().name() == seg)).collect();
    }
    cur
}

fn first<'a, 'i>(n: Node<'a, 'i>, path: &str) -> Option<Node<'a, 'i>> {
    path_all(n, path).into_iter().next()
}

fn kind<'a>(p: Node<'a, '_>) -> &'a str {
    match first(p, "typeDescriptor/type").or_else(|| first(p, "typeDescriptor/dependencyType/defaultType")) {
        Some(t) => t.attribute("name").unwrap_or(""),
        None => "Optional",
    }
}

fn take(node: Option<Node>, picks: &mut Vec<(String, String, bool)>) {
    for f in node.into_iter().flat_map(|n| n.children()).filter(|f| f.is_element()) {
        let tag = f.tag_name().name();
        if tag == "file" || tag == "folder" {
            let src = f.attribute("source").unwrap_or("");
            picks.push((src.into(), f.attribute("destination").unwrap_or(src).into(), tag == "folder"));
        }
    }
}

/// [(source file, game-relative posix path)] for everything the game can use in `tree`.
pub fn layout(tree: &Path, name: &str) -> Vec<(PathBuf, String)> {
    let all = walk(tree);
    let files: Vec<PathBuf> = all
        .iter()
        .filter(|p| is_plain_file(p)) // a link could point at any file on the PC
        .filter(|p| {
            let parts = parts(p, tree);
            !parts[..parts.len().saturating_sub(1)].iter().any(|x| x.to_lowercase() == "fomod")
        })
        .cloned()
        .collect();
    // The shallowest folder that holds game folders (archive/, r6/, ...) is the game root.
    let mut dirs: Vec<&Path> = std::iter::once(tree).chain(all.iter().filter(|p| p.is_dir()).map(PathBuf::as_path)).collect();
    dirs.sort_by_key(|d| d.components().count());
    for d in dirs {
        let kids: HashSet<String> = child_dirs(d).iter().map(|c| name_of(c).to_lowercase()).collect();
        if !ROOTS.iter().any(|r| kids.contains(*r)) {
            continue;
        }
        let cet_style = kids.contains("mods") && child_paths(&d.join("mods")).iter().any(|c| c.join("init.lua").exists());
        let mut out = vec![];
        for f in files.iter().filter(|f| f.starts_with(d)) {
            let rel = posix(f, d);
            let top = rel.split('/').next().unwrap_or("").to_lowercase();
            if top == "mods" && cet_style {
                out.push((f.clone(), format!("{CET_MODS}/{}", rel.split_once('/').map_or("", |x| x.1))));
            } else if ROOTS.contains(&top.as_str()) {
                out.push((f.clone(), rel));
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    // Loose files: place each kind where it loads from.
    let parent = |p: &PathBuf| p.parent().unwrap_or(tree).to_path_buf();
    let inits: Vec<PathBuf> = files.iter().filter(|p| name_of(p).to_lowercase() == "init.lua").map(parent).collect();
    let cet: Vec<&PathBuf> = inits.iter().filter(|d| !inits.iter().any(|o| o != *d && d.starts_with(o))).collect();
    let redmod: Vec<PathBuf> = files
        .iter()
        .filter(|p| name_of(p).to_lowercase() == "info.json" && parent(p).join("archives").is_dir())
        .map(parent)
        .collect();
    let dlls: HashSet<PathBuf> = files.iter().filter(|p| suffix(&name_of(p)) == ".dll").map(parent).collect();
    let reds: Vec<PathBuf> = files.iter().filter(|p| suffix(&name_of(p)) == ".reds").map(parent).collect();
    let reds_root = common(&reds);
    let label = |d: &Path| safe(&if d == tree { name.to_string() } else { name_of(d) });
    let mut out = vec![];
    for f in &files {
        let (fname, dir) = (name_of(f), parent(f));
        let ext = suffix(&fname);
        let rel = if let Some(home) = cet.iter().find(|d| f.starts_with(d)) {
            format!("{CET_MODS}/{}/{}", label(home), posix(f, home))
        } else if let Some(rm) = redmod.iter().find(|d| f.starts_with(d)) {
            format!("mods/{}/{}", label(rm), posix(f, rm))
        } else if dlls.contains(&dir) {
            format!("red4ext/plugins/{}/{fname}", label(&dir))
        } else if ext == ".archive" || ext == ".xl" {
            format!("archive/pc/mod/{fname}")
        } else if ext == ".reds" {
            format!("r6/scripts/{}/{}", safe(name), posix(f, &reds_root))
        } else if matches!(ext.as_str(), ".yaml" | ".yml" | ".tweak") {
            format!("r6/tweaks/{}/{fname}", safe(name))
        } else if ext == ".xml" {
            format!("r6/input/{fname}")
        } else if ext == ".ini" {
            format!("engine/config/platform/pc/{fname}")
        } else {
            continue;
        };
        out.push((f.clone(), rel));
    }
    out
}

/// Framework names the files need, from where they install and what kind they are.
pub fn needs_for<S: AsRef<str>>(rels: &[S]) -> BTreeSet<&'static str> {
    let mut need = BTreeSet::new();
    for r in rels.iter().map(|x| x.as_ref().to_lowercase()) {
        if r.starts_with("red4ext/plugins/") {
            need.insert("RED4ext");
        }
        if r.ends_with(".reds") {
            need.insert("redscript");
        }
        if r.ends_with(".xl") {
            need.insert("ArchiveXL");
        }
        if r.starts_with("r6/tweaks/") && [".yaml", ".yml", ".tweak"].iter().any(|e| r.ends_with(e)) {
            need.insert("TweakXL");
        }
        if r.starts_with(&format!("{CET_MODS}/")) {
            need.insert("Cyber Engine Tweaks");
        }
        if r.starts_with("r6/input/") {
            need.insert("Input Loader");
        }
    }
    need
}

/// Unpack `archive` into the staging folder `stage` in game layout. Returns the game-relative paths.
pub fn install(archive: &Path, stage: &Path, name: &str) -> Result<Vec<String>, String> {
    let tmp = Scratch::new()?;
    let tmp = tmp.0.as_path();
    let mut tree = tmp.join("x");
    fs::create_dir(&tree).map_err(|e| err(e, &tree))?;
    extract(archive, &tree, name)?;
    let src = fomod_tree(&tree, &tmp.join("fomod"))?.unwrap_or_else(|| tree.clone());
    let mut pairs = layout(&src, name);
    let tree_r = resolve(&tree);
    let mut inner: Vec<PathBuf> = walk(&tree)
        .into_iter()
        .filter(|p| matches!(suffix(&name_of(p)).as_str(), ".zip" | ".7z" | ".rar"))
        .filter(|p| p.is_file() && !is_link(p) && resolve(p).starts_with(&tree_r))
        .collect();
    inner.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    if pairs.is_empty() && inner.len() == 1 {
        // some mods pack their real archive inside another one
        tree = tmp.join("y");
        fs::create_dir(&tree).map_err(|e| err(e, &tree))?;
        extract(&inner[0], &tree, name)?;
        let src = fomod_tree(&tree, &tmp.join("fomod2"))?.unwrap_or_else(|| tree.clone());
        pairs = layout(&src, name);
    }
    if pairs.is_empty() {
        let more = if inner.len() > 1 {
            let names: Vec<String> = inner.iter().take(5).map(|p| name_of(p)).collect();
            format!(
                ". It holds {} other archives ({}): unpack it and add the one you want with 'Add files from this PC'",
                inner.len(),
                names.join(", ")
            )
        } else if let Some(one) = inner.first() {
            format!(", and neither has the archive inside it ({})", name_of(one))
        } else {
            String::new()
        };
        return Err(format!("{name} has no files the game can load{more}"));
    }
    let _ = fs::remove_dir_all(stage);
    for (f, rel) in &pairs {
        copy_to(f, &join(stage, rel))?;
    }
    Ok(pairs.into_iter().map(|(_, rel)| rel).collect::<BTreeSet<_>>().into_iter().collect())
}

/// A new temporary folder, deleted when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch, String> {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        loop {
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
            let p = std::env::temp_dir().join(format!("cp2077mm-{}-{nanos}-{n}", std::process::id()));
            match fs::create_dir(&p) {
                Ok(()) => return Ok(Scratch(p)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(err(e, &p)),
            }
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An I/O error with the path it is about.
pub(crate) fn err(e: io::Error, p: &Path) -> String {
    format!("{e}: '{}'", p.display())
}

/// `base` joined with a relative path written with `/` or `\`.
pub(crate) fn join(base: &Path, rel: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    p.extend(rel.split(['/', '\\']).filter(|s| !s.is_empty() && *s != "."));
    p
}

/// `p` relative to `base`, with `/` between the parts.
pub(crate) fn posix(p: &Path, base: &Path) -> String {
    parts(p, base).join("/")
}

/// Copy a file, making the target's folders first.
pub(crate) fn copy_to(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(d) = dst.parent() {
        fs::create_dir_all(d).map_err(|e| err(e, d))?;
    }
    fs::copy(src, dst).map(|_| ()).map_err(|e| err(e, src))
}

fn copy_tree(s: &Path, d: &Path) -> Result<(), String> {
    fs::create_dir_all(d).map_err(|e| err(e, d))?;
    for e in fs::read_dir(s).map_err(|e| err(e, s))?.flatten() {
        let (from, to) = (e.path(), d.join(e.file_name()));
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            copy_tree(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|e| err(e, &from))?;
        }
    }
    Ok(())
}

fn parts(p: &Path, base: &Path) -> Vec<String> {
    p.strip_prefix(base).map(|r| r.iter().map(|c| c.to_string_lossy().into_owned()).collect()).unwrap_or_default()
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Python's `Path.suffix`, lowercase: ".archive" for "a.archive", "" for ".reds" or "a.".
fn suffix(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => name[i..].to_lowercase(),
        _ => String::new(),
    }
}

/// A symlink or junction.
fn is_link(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
}

fn is_plain_file(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
}

fn child_paths(d: &Path) -> Vec<PathBuf> {
    fs::read_dir(d).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

fn child_dirs(d: &Path) -> Vec<PathBuf> {
    child_paths(d).into_iter().filter(|c| c.is_dir()).collect()
}

/// Every entry under `root`, parents before children, sorted by name in each folder.
/// Links are listed but never followed.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let Ok(rd) = fs::read_dir(root) else { return out };
    let mut kids: Vec<fs::DirEntry> = rd.flatten().collect();
    kids.sort_by_key(|e| e.file_name());
    for e in kids {
        let p = e.path();
        let dir = e.file_type().is_ok_and(|t| t.is_dir());
        out.push(p.clone());
        if dir {
            out.extend(walk(&p));
        }
    }
    out
}

/// The deepest folder that holds every path in `paths`.
fn common(paths: &[PathBuf]) -> PathBuf {
    let Some(mut c) = paths.first().cloned() else { return PathBuf::new() };
    for p in &paths[1..] {
        while !p.starts_with(&c) && c.pop() {}
    }
    c
}

/// Python's non-strict `Path.resolve()`: follow links in the part that exists, keep the rest as written.
fn resolve(p: &Path) -> PathBuf {
    let mut tail = vec![];
    let mut cur = p;
    loop {
        if let Ok(mut r) = cur.canonicalize() {
            r.extend(tail.iter().rev());
            return r;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(up), Some(n)) => {
                tail.push(n);
                cur = up;
            }
            _ => return p.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testkit::{TempDir, make_zip};

    const FOMOD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <moduleName>Test</moduleName>
  <requiredInstallFiles><folder source="core" destination="" /></requiredInstallFiles>
  <installSteps><installStep name="Pick"><optionalFileGroups>
    <group name="Colour" type="SelectExactlyOne"><plugins>
      <plugin name="Red"><files><file source="opts\red.archive" destination="archive\pc\mod\red.archive" /></files>
        <conditionFlags><flag name="col">red</flag></conditionFlags><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
      <plugin name="Blue"><files><file source="opts\blue.archive" destination="archive\pc\mod\" /></files>
        <conditionFlags><flag name="col">blue</flag></conditionFlags><typeDescriptor><type name="Recommended"/></typeDescriptor></plugin>
    </plugins></group>
    <group name="Extras" type="SelectAny"><plugins>
      <plugin name="Evil"><files><file source="opts\red.archive" destination="..\..\evil.archive" /></files>
        <typeDescriptor><type name="Required"/></typeDescriptor></plugin>
    </plugins></group>
  </optionalFileGroups></installStep></installSteps>
  <conditionalFileInstalls><patterns><pattern>
    <dependencies operator="And"><flagDependency flag="col" value="blue"/></dependencies>
    <files><file source="opts\blue.yaml" destination="r6\tweaks\blue.yaml" /></files>
  </pattern></patterns></conditionalFileInstalls>
</config>"#;

    fn run(t: &TempDir, files: &[(&str, &[u8])], name: &str, archive: &str) -> Result<Vec<String>, String> {
        install(&make_zip(t.path().join(archive), files), &t.path().join("stage"), name)
    }

    fn run_install(t: &TempDir, files: &[(&str, &[u8])]) -> Result<Vec<String>, String> {
        run(t, files, "My Mod", "m.zip")
    }

    fn write(p: &Path, body: &str) {
        fs::create_dir_all(p.parent().expect("parent")).expect("dirs");
        fs::write(p, body).expect("write");
    }

    #[test]
    fn rooted_inside_wrapper_folder() {
        // test_install.py Install.test_rooted_inside_wrapper_folder
        let t = TempDir::new();
        let rels = run_install(&t, &[("MyMod v2/archive/pc/mod/a.archive", b"x"), ("MyMod v2/r6/scripts/m/a.reds", b"x"), ("MyMod v2/readme.txt", b"x")]);
        assert_eq!(rels.unwrap(), ["archive/pc/mod/a.archive", "r6/scripts/m/a.reds"]);
        assert!(t.path().join("stage/archive/pc/mod/a.archive").exists());
    }

    #[test]
    fn loose_files() {
        // test_install.py Install.test_loose_files
        let t = TempDir::new();
        let files: Vec<(&str, &[u8])> = ["a.archive", "a.archive.xl", "src/main.reds", "src/sub/b.reds", "t.yaml", "keys.xml", "readme.md", "pic.png"]
            .iter()
            .map(|n| (*n, b"x".as_slice()))
            .collect();
        assert_eq!(
            run_install(&t, &files).unwrap(),
            [
                "archive/pc/mod/a.archive",
                "archive/pc/mod/a.archive.xl",
                "r6/input/keys.xml",
                "r6/scripts/My Mod/main.reds",
                "r6/scripts/My Mod/sub/b.reds",
                "r6/tweaks/My Mod/t.yaml"
            ]
        );
    }

    #[test]
    fn cet_mod_loose_and_cet_relative() {
        // test_install.py Install.test_cet_mod_loose_and_cet_relative
        let t = TempDir::new();
        let rels = run_install(&t, &[("Cool/init.lua", b"x"), ("Cool/modules/x/init.lua", b"x"), ("Cool/data.json", b"x")]);
        assert_eq!(
            rels.unwrap(),
            [
                "bin/x64/plugins/cyber_engine_tweaks/mods/Cool/data.json",
                "bin/x64/plugins/cyber_engine_tweaks/mods/Cool/init.lua",
                "bin/x64/plugins/cyber_engine_tweaks/mods/Cool/modules/x/init.lua"
            ]
        );
        let rels = run(&t, &[("mods/Cool/init.lua", b"x")], "My Mod", "n.zip");
        assert_eq!(rels.unwrap(), ["bin/x64/plugins/cyber_engine_tweaks/mods/Cool/init.lua"]);
    }

    #[test]
    fn red4ext_plugin_and_redmod() {
        // test_install.py Install.test_red4ext_plugin_and_redmod
        let t = TempDir::new();
        let rels = run_install(&t, &[("Plug/Plug.dll", b"x"), ("Plug/cfg.toml", b"x")]);
        assert_eq!(rels.unwrap(), ["red4ext/plugins/Plug/Plug.dll", "red4ext/plugins/Plug/cfg.toml"]);
        let rels = run(&t, &[("mods/Big/info.json", b"{}"), ("mods/Big/archives/b.archive", b"x")], "My Mod", "r.zip");
        assert_eq!(rels.unwrap(), ["mods/Big/archives/b.archive", "mods/Big/info.json"]);
    }

    #[test]
    fn fomod_defaults_flags_and_path_guard() {
        // test_install.py Install.test_fomod_defaults_flags_and_path_guard
        let t = TempDir::new();
        let rels = run_install(
            &t,
            &[
                ("fomod/ModuleConfig.xml", FOMOD.as_bytes()),
                ("core/r6/scripts/c.reds", b"x"),
                ("opts/red.archive", b"r"),
                ("opts/blue.archive", b"b"),
                ("opts/blue.yaml", b"y"),
            ],
        );
        assert_eq!(rels.unwrap(), ["archive/pc/mod/blue.archive", "r6/scripts/c.reds", "r6/tweaks/blue.yaml"]);
    }

    #[test]
    fn fomod_cannot_write_outside_its_folder() {
        // test_install.py Install.test_fomod_cannot_write_outside_its_folder
        let t = TempDir::new();
        let (tree, out) = (t.path().join("a/b/tree"), t.path().join("a/b/out"));
        for (name, body) in [("fomod/ModuleConfig.xml", FOMOD), ("opts/red.archive", "r"), ("opts/blue.archive", "b")] {
            write(&tree.join(name), body);
        }
        fomod_tree(&tree, &out).unwrap();
        assert!(!t.path().join("a/evil.archive").exists()); // "..\..\evil.archive" from out
        assert!(!walk(t.path()).iter().any(|p| name_of(p) == "evil.archive"));
    }

    #[test]
    fn archive_name_cannot_inject_tar_options() {
        // test_install.py Install.test_archive_name_cannot_inject_tar_options
        // A full-width quote becomes a real quote in tar's ANSI command line ("best-fit"); "--version" must stay a name.
        let t = TempDir::new();
        let rels = run(&t, &[("a.archive", b"x")], "My Mod", "mod\u{FF02} --version \u{FF02}.zip");
        assert_eq!(rels.unwrap(), ["archive/pc/mod/a.archive"]);
    }

    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn read_only_archive_installs_and_stays_unchanged() {
        // test_install.py Install.test_read_only_archive_installs_and_stays_unchanged
        let t = TempDir::new();
        let z = make_zip(t.path().join("ro.zip"), &[("a.archive", b"x")]);
        let mut perm = fs::metadata(&z).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(&z, perm.clone()).unwrap();
        let rels = install(&z, &t.path().join("stage"), "RO");
        let still = fs::metadata(&z).unwrap().permissions().readonly(); // nothing touched the user's file
        perm.set_readonly(false);
        fs::set_permissions(&z, perm).unwrap();
        assert_eq!(rels.unwrap(), ["archive/pc/mod/a.archive"]);
        assert!(still);
    }

    #[test]
    fn fomod_folder_with_a_junction_is_skipped() {
        // test_install.py Install.test_fomod_folder_with_a_junction_is_skipped
        let t = TempDir::new();
        let outside = t.path().join("outside");
        write(&outside.join("secret.txt"), "private");
        let (tree, out) = (t.path().join("tree"), t.path().join("out"));
        write(&tree.join("opts/ok.reds"), "x");
        let r = Command::new(shared::sys32().join("cmd.exe"))
            .args(["/c", "mklink", "/J"])
            .arg(tree.join("opts/link"))
            .arg(&outside)
            .output()
            .unwrap();
        if !r.status.success() {
            return; // could not create a junction
        }
        write(&tree.join("fomod/ModuleConfig.xml"), r#"<config><requiredInstallFiles><folder source="opts" destination="r6/scripts/m" /></requiredInstallFiles></config>"#);
        fomod_tree(&tree, &out).unwrap();
        assert!(!out.join("r6").exists()); // the whole pick is skipped, so nothing goes through the link
        assert!(!walk(&out).iter().any(|p| name_of(p) == "secret.txt"));
    }

    #[test]
    fn inside_refuses_every_way_out() {
        // test_install.py Install.test_inside_refuses_every_way_out
        for bad in [r"..\..\evil.archive", "../x", r"\Users\Public\evil.archive", "/Users/Public/evil.archive", "D:evil.dll", "C:/x", r"C:\x", r"\\server\share\x", "a/../../x"] {
            assert!(!inside(bad), "{bad}");
        }
        for ok in ["", r"archive\pc\mod\", "archive/pc/mod/a.archive", "Option A", r"r6\scripts"] {
            assert!(inside(ok), "{ok}");
        }
    }

    #[test]
    fn fomod_that_picks_nothing_gives_a_clear_error() {
        // test_install.py Install.test_fomod_that_picks_nothing_gives_a_clear_error
        let t = TempDir::new();
        let empty = FOMOD
            .replace(r#"type="SelectExactlyOne""#, r#"type="SelectAny""#)
            .replace(r#""Recommended""#, r#""Optional""#)
            .replace(r#"<type name="Required"/>"#, r#"<type name="Optional"/>"#)
            .replace(r#"<folder source="core" destination="" />"#, "");
        let e = run_install(&t, &[("fomod/ModuleConfig.xml", empty.as_bytes()), ("core/x.reds", b"x")]).unwrap_err();
        assert!(e.contains("no files the game can load"), "{e}");
    }

    #[test]
    fn fomod_with_entities_is_ignored() {
        // test_install.py Install.test_fomod_with_entities_is_ignored
        let t = TempDir::new();
        let bomb = FOMOD.replace("<config", r#"<!DOCTYPE c [<!ENTITY a "aaaa">]><config"#);
        let rels = run_install(&t, &[("fomod/ModuleConfig.xml", bomb.as_bytes()), ("x.archive", b"x")]);
        assert_eq!(rels.unwrap(), ["archive/pc/mod/x.archive"]);
    }

    #[test]
    fn test_7z_unpacks() {
        // test_install.py Install.test_7z_unpacks
        let t = TempDir::new();
        let src = t.path().join("src");
        write(&src.join("archive/pc/mod/z.archive"), "x");
        let z = t.path().join("z.7z");
        let ok = Command::new(&*TAR).args(["--format", "7zip", "-cf"]).arg(&z).arg("-C").arg(&src).arg("archive").status().unwrap();
        assert!(ok.success());
        assert_eq!(install(&z, &t.path().join("stage"), "Z").unwrap(), ["archive/pc/mod/z.archive"]);
    }

    #[test]
    fn symlinks_in_archives_are_not_installed() {
        // test_install.py Install.test_symlinks_in_archives_are_not_installed
        let t = TempDir::new();
        let tree = t.path().join("tree");
        write(&tree.join("archive/pc/mod/ok.archive"), "x");
        let secret = t.path().join("secret.txt");
        write(&secret, "private");
        if std::os::windows::fs::symlink_file(&secret, tree.join("archive/pc/mod/leak.archive")).is_err() {
            return; // this Windows account cannot create symlinks
        }
        let rels: Vec<String> = layout(&tree, "M").into_iter().map(|(_, r)| r).collect();
        assert_eq!(rels, ["archive/pc/mod/ok.archive"]);
    }

    #[test]
    fn archive_inside_an_archive() {
        // test_install.py Install.test_archive_inside_an_archive
        let t = TempDir::new();
        let one = fs::read(make_zip(t.path().join("inner.zip"), &[("archive/pc/mod/in.archive", b"x")])).unwrap();
        assert_eq!(run_install(&t, &[("Main/inner.zip", &one)]).unwrap(), ["archive/pc/mod/in.archive"]);
        let e = run(&t, &[("b.zip", &one), ("a.zip", &one)], "My Mod", "two.zip").unwrap_err();
        assert!(e.contains("holds 2 other archives (a.zip, b.zip)"), "{e}");
        let empty = fs::read(make_zip(t.path().join("empty.zip"), &[("readme.txt", b"x")])).unwrap();
        let e = run(&t, &[("e.zip", &empty)], "My Mod", "one.zip").unwrap_err();
        assert!(e.ends_with("no files the game can load, and neither has the archive inside it (e.zip)"), "{e}");
    }

    #[test]
    fn nothing_usable() {
        // test_install.py Install.test_nothing_usable
        let t = TempDir::new();
        let e = run_install(&t, &[("readme.txt", b"x")]).unwrap_err();
        assert!(e.starts_with("My Mod has no files"), "{e}"); // errors name the mod, not the temp file
    }

    #[test]
    fn broken_archive_names_the_mod() {
        // test_install.py Install.test_broken_archive_names_the_mod
        let t = TempDir::new();
        let bad = t.path().join("bad.zip");
        fs::write(&bad, b"not a zip").unwrap();
        let e = install(&bad, &t.path().join("stage"), "Broken Mod").unwrap_err();
        assert!(e.starts_with("Could not unpack Broken Mod"), "{e}");
    }

    #[test]
    fn needs() {
        // test_install.py Install.test_needs
        let got = needs_for(&[
            "red4ext/plugins/X/x.dll",
            "red4ext/plugins/X/Scripts/a.reds",
            "archive/pc/mod/a.xl",
            "r6/tweaks/a.yaml",
            "bin/x64/plugins/cyber_engine_tweaks/mods/m/init.lua",
            "r6/input/k.xml",
        ]);
        let want: BTreeSet<&str> = ["RED4ext", "redscript", "ArchiveXL", "TweakXL", "Cyber Engine Tweaks", "Input Loader"].into();
        assert_eq!(got, want);
        assert!(needs_for(&["archive/pc/mod/a.archive"]).is_empty());
    }
}
