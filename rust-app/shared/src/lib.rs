//! App data folder, settings and the installed-mod list, kept in one JSON file (`state.json`).
//! The file format is the same as the Python app's, so a user can switch between them.
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::io;
use std::path::{Path, PathBuf};

pub mod win;

/// Window title, MCP server and the Nexus Application-Version header.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GAME_EXE: &str = "bin/x64/Cyberpunk2077.exe";

/// Frameworks most mods need: (name, GitHub repo, Nexus mod id). They all publish GitHub releases,
/// so they download with no clicks and no account.
pub const FRAMEWORKS: &[(&str, &str, u64)] = &[
    ("RED4ext", "wopss/RED4ext", 2380),
    ("Cyber Engine Tweaks", "maximegmd/CyberEngineTweaks", 107),
    ("redscript", "jac3km4/redscript", 1511),
    ("ArchiveXL", "psiberx/cp2077-archive-xl", 4198),
    ("TweakXL", "psiberx/cp2077-tweak-xl", 4197),
    ("Codeware", "psiberx/cp2077-codeware", 7780),
    ("Input Loader", "jackhumbert/cyberpunk2077-input-loader", 4575),
    ("Mod Settings", "jackhumbert/mod_settings", 4885),
];

/// Framework name for a Nexus mod id.
pub fn fw_by_nexus(id: u64) -> Option<&'static str> {
    FRAMEWORKS.iter().find(|f| f.2 == id).map(|f| f.0)
}

/// Framework name for a GitHub repo ("owner/name", any case).
pub fn fw_by_repo(repo: &str) -> Option<&'static str> {
    FRAMEWORKS.iter().find(|f| f.1.eq_ignore_ascii_case(repo)).map(|f| f.0)
}

/// `%SystemRoot%\System32`: Windows programs are always called by full path.
pub fn sys32() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join("System32")
}

/// The data folder: `CP2077MM_DATA`, else `%LOCALAPPDATA%\CP2077ModManager`.
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CP2077MM_DATA").filter(|d| !d.is_empty()) {
        return d.into();
    }
    let base = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("USERPROFILE")).unwrap_or_default();
    PathBuf::from(base).join("CP2077ModManager")
}

/// Any JSON value as text (old or hand-edited state files may hold a number or null).
fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s,
        Value::Null => String::new(),
        v => v.to_string(),
    })
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub game_dir: String,
    /// Secret: never print, log or commit it.
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "yes")]
    pub virus_scan: bool,
    #[serde(default = "yes")]
    pub auto_fix: bool,
    #[serde(default = "yes")]
    pub include_optional: bool,
    #[serde(default = "yes")]
    pub auto_click: bool,
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Settings this version does not know; kept so nothing is lost.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_theme() -> String {
    "Cyberpunk".into()
}

impl Default for Settings {
    fn default() -> Self {
        serde_json::from_value(Value::Object(Map::new())).expect("defaults")
    }
}

/// One installed mod. `files` are game-relative posix paths.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mod {
    pub id: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub version: String,
    #[serde(default)]
    pub enabled: bool,
    /// `{"type": "nexus"|"github"|"url"|"file", ...}` as `nexus::parse_link` made it.
    #[serde(default)]
    pub source: Value,
    #[serde(default)]
    pub files: Vec<String>,
    /// Framework names the files need.
    #[serde(default)]
    pub needs: Vec<String>,
    /// Nexus requirements: `[{"mod_id", "name", ...}]`.
    #[serde(default)]
    pub requires: Vec<Value>,
    /// The framework name when this mod is one.
    #[serde(default)]
    pub framework: Option<String>,
    /// Unix time of the install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed: Option<f64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Mod {
    /// A mod record with the defaults the tests and file installs use.
    pub fn new(id: &str, files: &[&str]) -> Mod {
        Mod {
            id: id.into(),
            name: id.into(),
            version: "1".into(),
            enabled: true,
            source: serde_json::json!({"type": "file"}),
            files: files.iter().map(|f| f.to_string()).collect(),
            needs: vec![],
            requires: vec![],
            framework: None,
            installed: None,
            extra: Map::new(),
        }
    }

    /// `source["type"]`, or "".
    pub fn source_type(&self) -> &str {
        self.source.get("type").and_then(Value::as_str).unwrap_or("")
    }
}

/// A mod's file types for people: "Framework", or e.g. "Archive, Redscript".
pub fn kind_of(m: &Mod) -> String {
    if m.framework.as_deref().is_some_and(|f| !f.is_empty()) {
        return "Framework".into();
    }
    let mut tags: Vec<&str> = vec![];
    for r in &m.files {
        let r = r.to_lowercase();
        let tag = if r.starts_with("mods/") {
            "REDmod"
        } else if r.contains("cyber_engine_tweaks/mods/") {
            "CET"
        } else if r.starts_with("red4ext/") {
            "RED4ext"
        } else if r.ends_with(".reds") {
            "Redscript"
        } else if r.starts_with("r6/tweaks/") {
            "Tweak"
        } else if r.starts_with("archive/") {
            "Archive"
        } else {
            "Other"
        };
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    let named: Vec<&str> = tags.iter().copied().filter(|t| *t != "Other").take(3).collect();
    if named.is_empty() { tags.join(", ") } else { named.join(", ") }
}

/// The whole `state.json`. Field order matches the Python app's file.
#[derive(Serialize, Deserialize, Default)]
struct StateFile {
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    mods: Vec<Mod>,
    #[serde(default)]
    deployed: IndexMap<String, Option<String>>,
    #[serde(default)]
    deploy_game: String,
    #[serde(default)]
    modlist_tracked: Option<bool>,
    #[serde(default)]
    doctor: Map<String, Value>,
    #[serde(default)]
    packs: IndexMap<String, Vec<(String, bool)>>,
    #[serde(default)]
    pack: String,
    #[serde(default)]
    pack_info: Map<String, Value>,
}

/// `mods[0]` is the lowest priority; the last mod in the list wins every conflict.
/// Callers share one Store behind a lock and call `save()` after each change.
pub struct Store {
    pub root: PathBuf,
    pub downloads: PathBuf,
    /// Staging folder: one subfolder per mod id, in game layout.
    pub staging: PathBuf,
    pub backups: PathBuf,
    pub file: PathBuf,
    pub settings: Settings,
    pub mods: Vec<Mod>,
    /// Lowercase game-relative path -> mod id (None: copy again).
    pub deployed: IndexMap<String, Option<String>>,
    /// The game folder `deployed` describes.
    pub deploy_game: String,
    /// False only for a state file written by v1.0.0, until its first v1.0.1 deploy decides who owns modlist.txt.
    pub modlist_tracked: bool,
    /// last_launch, last_exit, last_good, last_good_game, bisect, history, ignored, crashes, clean_errors.
    pub doctor: Map<String, Value>,
    /// Mod pack name -> [(mod id, on)], lowest priority first.
    pub packs: IndexMap<String, Vec<(String, bool)>>,
    /// The pack the mod list was last set to ("" = none).
    pub pack: String,
    /// Nexus packs only: name -> {"url", "names": {mod id: mod name}}.
    pub pack_info: Map<String, Value>,
}

impl Store {
    /// The store in the default data folder.
    pub fn open_default() -> io::Result<Store> {
        Store::open(data_dir())
    }

    /// Load `state.json` from `root` (a missing file is an empty store). A file that cannot be read is an
    /// error, so it is never overwritten with an empty list.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Store> {
        let root = root.as_ref().to_path_buf();
        let (downloads, staging, backups) = (root.join("downloads"), root.join("mods"), root.join("backups"));
        for p in [&downloads, &staging, &backups] {
            std::fs::create_dir_all(p)?;
        }
        let file = root.join("state.json");
        let exists = file.exists();
        let d: StateFile = if exists {
            let text = std::fs::read_to_string(&file)?;
            serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("state.json: {e}")))?
        } else {
            StateFile::default()
        };
        Ok(Store {
            root,
            downloads,
            staging,
            backups,
            file,
            settings: d.settings,
            mods: d.mods,
            deployed: d.deployed,
            deploy_game: d.deploy_game,
            modlist_tracked: d.modlist_tracked.unwrap_or(!exists),
            doctor: d.doctor,
            packs: d.packs,
            pack: d.pack,
            pack_info: d.pack_info,
        })
    }

    /// Write `state.json` through a temporary file, so a crash never leaves half a file.
    pub fn save(&self) -> io::Result<()> {
        let body = serde_json::json!({
            "settings": self.settings, "mods": self.mods, "deployed": self.deployed, "deploy_game": self.deploy_game,
            "modlist_tracked": self.modlist_tracked, "doctor": self.doctor, "packs": self.packs, "pack": self.pack,
            "pack_info": self.pack_info,
        });
        let tmp = self.file.with_extension("tmp");
        std::fs::write(&tmp, pretty(&body))?;
        std::fs::rename(&tmp, &self.file)
    }

    pub fn mod_by_id(&self, id: &str) -> Option<&Mod> {
        self.mods.iter().find(|m| m.id == id)
    }

    pub fn mod_by_id_mut(&mut self, id: &str) -> Option<&mut Mod> {
        self.mods.iter_mut().find(|m| m.id == id)
    }

    pub fn enabled(&self) -> Vec<&Mod> {
        self.mods.iter().filter(|m| m.enabled).collect()
    }

    /// The game folder, when the setting points at a folder with the game's exe.
    pub fn game(&self) -> Option<PathBuf> {
        let g = &self.settings.game_dir;
        (!g.is_empty() && Path::new(g).join(GAME_EXE).exists()).then(|| PathBuf::from(g))
    }
}

/// JSON with a one-space indent, like Python's `json.dumps(body, indent=1)`.
pub fn pretty(v: &Value) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(b" "));
    v.serialize(&mut ser).expect("JSON");
    String::from_utf8(out).expect("UTF-8")
}

/// Look in Steam libraries, GOG and Epic for the Cyberpunk 2077 folder.
pub fn find_game() -> String {
    let mut cands: Vec<PathBuf> = vec![];
    if let Some(steam) = win::reg_get(win::HKCU, r"Software\Valve\Steam", "SteamPath") {
        let steam = PathBuf::from(steam);
        let vdf = steam.join("steamapps/libraryfolders.vdf");
        let mut libs = vec![steam.clone()];
        if let Ok(bytes) = std::fs::read(&vdf) {
            let text = String::from_utf8_lossy(&bytes);
            let re = regex::Regex::new(r#""path"\s+"([^"]+)""#).expect("regex");
            libs.extend(re.captures_iter(&text).map(|c| PathBuf::from(c[1].replace(r"\\", r"\"))));
        }
        cands.extend(libs.iter().map(|l| l.join("steamapps/common/Cyberpunk 2077")));
    }
    if let Some(p) = win::reg_get(win::HKLM, r"SOFTWARE\WOW6432Node\GOG.com\Games\1423049311", "path") {
        cands.push(p.into());
    }
    let pd = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    if let Ok(dir) = std::fs::read_dir(PathBuf::from(pd).join(r"Epic\EpicGamesLauncher\Data\Manifests")) {
        for item in dir.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("item"))) {
            let Ok(text) = std::fs::read_to_string(&item) else { continue };
            let Ok(d) = serde_json::from_str::<Value>(&text) else { continue };
            let name = d.get("DisplayName").and_then(Value::as_str).unwrap_or("");
            if let (true, Some(loc)) = (name.to_lowercase().contains("cyberpunk"), d.get("InstallLocation").and_then(Value::as_str)) {
                cands.push(loc.into());
            }
        }
    }
    cands.extend([
        r"C:\Program Files (x86)\Steam\steamapps\common\Cyberpunk 2077",
        r"C:\GOG Games\Cyberpunk 2077",
        r"C:\Program Files\Epic Games\Cyberpunk2077",
    ].map(PathBuf::from));
    cands.into_iter().find(|c| c.join(GAME_EXE).exists()).map(|c| c.display().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_store_is_empty_and_tracks_modlist() {
        let t = tempdir();
        let s = Store::open(&t).unwrap();
        assert!(s.mods.is_empty() && s.modlist_tracked && s.settings.virus_scan && s.settings.theme == "Cyberpunk");
        assert!(s.staging.is_dir() && s.downloads.is_dir() && s.backups.is_dir());
    }

    #[test]
    fn python_state_file_round_trips() {
        let t = tempdir();
        let py = r#"{
 "settings": {"game_dir": "", "api_key": "", "virus_scan": false, "auto_fix": true, "include_optional": true, "theme": "Night", "future": 3},
 "mods": [{"id": "nexus-1-2", "name": "A", "version": 2.1, "enabled": true, "source": {"type": "nexus", "mod_id": 1, "file_id": 2},
   "files": ["archive/pc/mod/a.archive"], "needs": [], "requires": [{"mod_id": 5, "name": "B"}], "framework": null,
   "installed": 1700000000.5, "nexus_status": "x"}],
 "deployed": {"archive/pc/mod/a.archive": "nexus-1-2", "x": null},
 "deploy_game": "C:\\Games\\Cyberpunk 2077",
 "doctor": {"history": [], "bisect": null},
 "packs": {"P": [["nexus-1-2", true]]}, "pack": "P", "pack_info": {}
}"#;
        std::fs::write(t.join("state.json"), py).unwrap();
        let s = Store::open(&t).unwrap();
        assert!(!s.modlist_tracked, "a v1.0.0 file has no modlist_tracked");
        assert!(!s.settings.virus_scan && s.settings.auto_click && s.settings.extra["future"] == 3);
        let m = &s.mods[0];
        assert_eq!((m.version.as_str(), m.framework.clone(), m.extra["nexus_status"].as_str()), ("2.1", None, Some("x")));
        assert_eq!(s.deployed["x"], None);
        assert_eq!(s.packs["P"], vec![("nexus-1-2".to_string(), true)]);
        s.save().unwrap();
        let back: Value = serde_json::from_str(&std::fs::read_to_string(t.join("state.json")).unwrap()).unwrap();
        assert_eq!(back["mods"][0]["requires"][0]["mod_id"], 5);
        assert_eq!(back["mods"][0]["framework"], Value::Null);
        assert_eq!(back["mods"][0]["installed"], 1700000000.5);
        assert_eq!(back["settings"]["future"], 3);
        assert_eq!(back["packs"]["P"][0][1], true);
        let keys: Vec<&str> = back.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["settings", "mods", "deployed", "deploy_game", "modlist_tracked", "doctor", "packs", "pack", "pack_info"]);
        let again = Store::open(&t).unwrap();
        assert_eq!(again.mods, s.mods);
    }

    #[test]
    fn broken_state_file_is_an_error_not_an_empty_list() {
        let t = tempdir();
        std::fs::write(t.join("state.json"), "{\"mods\": [").unwrap();
        assert!(Store::open(&t).is_err());
    }

    #[test]
    fn game_needs_the_exe() {
        let t = tempdir();
        let mut s = Store::open(&t).unwrap();
        s.settings.game_dir = t.display().to_string();
        assert!(s.game().is_none());
        std::fs::create_dir_all(t.join("bin/x64")).unwrap();
        std::fs::write(t.join(GAME_EXE), b"MZ").unwrap();
        assert_eq!(s.game(), Some(t.clone()));
    }

    #[test]
    fn kinds() {
        let mut m = Mod::new("a", &["archive/pc/mod/a.archive", "r6/scripts/x.reds", "bin/x64/readme.txt"]);
        assert_eq!(kind_of(&m), "Archive, Redscript");
        m.files = vec!["bin/x64/readme.txt".into()];
        assert_eq!(kind_of(&m), "Other");
        m.framework = Some("RED4ext".into());
        assert_eq!(kind_of(&m), "Framework");
    }

    #[test]
    fn framework_lookups() {
        assert_eq!(fw_by_nexus(2380), Some("RED4ext"));
        assert_eq!(fw_by_repo("PSIBERX/cp2077-archive-xl"), Some("ArchiveXL"));
        assert_eq!(fw_by_repo("x/y"), None);
    }

    fn tempdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("cp2077mm-shared-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
