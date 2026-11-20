//! Test helpers shared by every crate's tests (the Rust form of the Python `tests/helpers.py`).
use shared::{Mod, Store};
use std::io::Write;
use std::path::{Path, PathBuf};

/// A new empty folder that is deleted when dropped.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("cp2077mm-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        TempDir(d)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Default for TempDir {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write a zip at `path` holding `files` (inner path, body).
pub fn make_zip(path: impl AsRef<Path>, files: &[(&str, &[u8])]) -> PathBuf {
    let path = path.as_ref().to_path_buf();
    let mut z = zip::ZipWriter::new(std::fs::File::create(&path).expect("zip file"));
    for (name, body) in files {
        z.start_file(*name, zip::write::SimpleFileOptions::default()).expect("zip entry");
        z.write_all(body).expect("zip body");
    }
    z.finish().expect("zip finish");
    path
}

/// Bytes of a minimal .archive (RDAR v12) holding the given resource hashes.
pub fn rdar(hashes: &[u64]) -> Vec<u8> {
    let mut entries = vec![];
    for h in hashes {
        entries.extend(h.to_le_bytes());
        entries.extend(0i64.to_le_bytes());
        entries.extend([0u8; 5 * 4]);
        entries.extend([0u8; 20]);
    }
    let mut idx = vec![];
    idx.extend(8u32.to_le_bytes());
    idx.extend((entries.len() as u32 + 20).to_le_bytes());
    idx.extend(0u64.to_le_bytes());
    idx.extend((hashes.len() as u32).to_le_bytes());
    idx.extend(0u32.to_le_bytes());
    idx.extend(0u32.to_le_bytes());
    idx.extend(entries);
    let mut out = vec![];
    out.extend(0x5241_4452u32.to_le_bytes());
    out.extend(12u32.to_le_bytes());
    out.extend(40u64.to_le_bytes());
    out.extend((idx.len() as u32).to_le_bytes());
    out.extend(0u64.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend((40 + idx.len() as u64).to_le_bytes());
    out.extend(idx);
    out
}

/// A fake game folder `<root>/Cyberpunk 2077` with the exe and `archive/pc/mod`.
pub fn fake_game(root: impl AsRef<Path>) -> PathBuf {
    let g = root.as_ref().join("Cyberpunk 2077");
    std::fs::create_dir_all(g.join("bin/x64")).expect("bin");
    std::fs::write(g.join("bin/x64/Cyberpunk2077.exe"), b"MZ").expect("exe");
    std::fs::create_dir_all(g.join("archive/pc/mod")).expect("archive");
    g
}

/// A mod record like Python's `helpers.mod(mid, files)`; change fields on the result for the `**kw` cases.
pub fn mod_rec(id: &str, files: &[&str]) -> Mod {
    Mod::new(id, files)
}

/// Write staging files for mod `m` (body `"<id>:<rel>"` unless given) and add it to the store.
pub fn stage(store: &mut Store, m: Mod, bodies: &[(&str, &[u8])]) -> Mod {
    for rel in &m.files {
        let p = store.staging.join(&m.id).join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("dirs");
        let body = bodies.iter().find(|(r, _)| r == rel).map(|(_, b)| b.to_vec()).unwrap_or_else(|| format!("{}:{rel}", m.id).into_bytes());
        std::fs::write(p, body).expect("stage file");
    }
    store.mods.push(m.clone());
    m
}

/// A store in a fresh temp folder (keep the TempDir alive while the store is used).
pub fn temp_store() -> (TempDir, Store) {
    let t = TempDir::new();
    let s = Store::open(t.path().join("data")).expect("store");
    (t, s)
}
