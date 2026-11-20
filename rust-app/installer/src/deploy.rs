//! Copy enabled mods from staging into the game folder. Lower in the mod list wins.
//! Game files a mod replaces are backed up first and put back when no mod needs that path.
use crate::install::{copy_to, err, join, posix};
use shared::{GAME_EXE, Store};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const ARCHIVES: &str = "archive/pc/mod";
pub const MODLIST: &str = "archive/pc/mod/modlist.txt";
/// `deployed[MODLIST] = LOAD_ORDER` while this app owns modlist.txt.
pub const LOAD_ORDER: &str = "__load_order__";
pub const KEEP: [&str; 10] = [
    "archive/pc/mod",
    "r6/scripts",
    "r6/tweaks",
    "r6/input",
    "red4ext/plugins",
    "bin/x64",
    "bin/x64/plugins",
    "bin/x64/plugins/cyber_engine_tweaks/mods",
    "mods",
    "engine/config/platform/pc",
];

/// (lowercase key, mod id, game-relative path) for every file to deploy, in first-seen key order.
/// Later mods overwrite earlier ones.
pub fn plan(store: &Store) -> Vec<(String, String, String)> {
    let mut want: Vec<(String, String, String)> = vec![];
    let mut at: HashMap<String, usize> = HashMap::new();
    for m in store.enabled() {
        for rel in &m.files {
            let key = rel.to_lowercase();
            if key == MODLIST {
                continue; // the load order file is written by deploy(), never copied from a mod
            }
            let v = (key.clone(), m.id.clone(), rel.clone());
            match at.get(&key) {
                Some(&i) => want[i] = v,
                None => {
                    at.insert(key, want.len());
                    want.push(v);
                }
            }
        }
    }
    want
}

/// A `.archive` directly in `archive/pc/mod`.
fn is_archive(r: &str) -> bool {
    let l = r.to_lowercase();
    l.starts_with(&format!("{ARCHIVES}/")) && l.ends_with(".archive") && r.matches('/').count() == 3
}

fn remove(store: &mut Store, game: &Path, key: &str) -> Result<(), String> {
    let (dst, bak) = (join(game, key), join(&store.backups, key));
    if bak.exists() {
        fs::rename(&bak, &dst).map_err(|e| err(e, &bak))?;
    } else {
        match fs::remove_file(&dst) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(err(e, &dst)),
            _ => {}
        }
        let mut d = dst.clone();
        while d.pop() && d != game && !KEEP.contains(&posix(&d, game).to_lowercase().as_str()) && d.is_dir() && is_empty(&d) {
            fs::remove_dir(&d).map_err(|e| err(e, &d))?;
        }
    }
    store.deployed.shift_remove(key);
    Ok(())
}

fn is_empty(d: &Path) -> bool {
    fs::read_dir(d).is_ok_and(|mut r| r.next().is_none())
}

/// The game folder changed since the last deploy: take the mods out of the old folder first.
fn switch_game(store: &mut Store, game: &Path) -> Result<(), String> {
    let norm = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\");
    if !store.deploy_game.is_empty() && !store.deployed.is_empty() {
        let old: PathBuf = Path::new(&store.deploy_game).components().collect();
        if norm(&old) != norm(game) {
            if old.join(GAME_EXE).exists() {
                let keys: Vec<String> = store.deployed.keys().cloned().collect();
                for key in keys {
                    remove(store, &old, &key)?;
                }
            } else {
                // the old folder is gone: its backups can never go back, so keep them aside, unused
                store.deployed.clear();
                let backups = store.backups.clone();
                if fs::read_dir(&backups).map_err(|e| err(e, &backups))?.next().is_some() {
                    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                    fs::rename(&backups, backups.with_file_name(format!("backups-{secs}"))).map_err(|e| err(e, &backups))?;
                    fs::create_dir(&backups).map_err(|e| err(e, &backups))?;
                }
            }
        }
    }
    store.deploy_game = game.to_string_lossy().into_owned();
    Ok(())
}

/// (managed, others): archive file names in load order. The game loads archive/pc/mod in modlist.txt
/// order and the first archive loaded wins, so the highest-priority mod goes first. Archives this app
/// does not manage go last.
pub fn load_order(store: &Store, game: &Path) -> (Vec<String>, Vec<String>) {
    let mut managed: Vec<String> = vec![];
    for m in store.enabled().into_iter().rev() {
        for r in m.files.iter().filter(|r| is_archive(r)) {
            let n = r.rsplit(['/', '\\']).next().unwrap_or("").to_string();
            if !managed.contains(&n) {
                managed.push(n);
            }
        }
    }
    let low: HashSet<String> = managed.iter().map(|n| n.to_lowercase()).collect();
    let mut others: Vec<String> = fs::read_dir(join(game, ARCHIVES))
        .map(|r| {
            r.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.to_lowercase().ends_with(".archive") && !low.contains(&n.to_lowercase()))
                .collect()
        })
        .unwrap_or_default();
    others.sort();
    (managed, others)
}

/// Sync the game folder with the enabled mods. Returns how many files were copied or removed.
/// A file is copied only when its owner changed or it is missing, so configs that the game or a mod
/// edits in place are kept. State is saved even when a copy fails part way.
/// The caller holds the store's lock.
pub fn deploy(store: &mut Store) -> Result<usize, String> {
    let game: PathBuf = store.game().ok_or_else(|| "Set the Cyberpunk 2077 folder in Settings first.".to_string())?.components().collect();
    let done = sync(store, &game);
    store.save().map_err(|e| e.to_string())?;
    done
}

fn sync(store: &mut Store, game: &Path) -> Result<usize, String> {
    if !store.modlist_tracked {
        // A v1.0.0 state file: decide once, before anything is copied or moved. v1.0.0 rewrote modlist.txt
        // without recording it whenever an archive was in the folder. If one of its own archives is on, the
        // file there is its list, so it is ours to replace or remove (also in an old game folder that
        // switch_game below empties).
        if !store.deployed.contains_key(MODLIST) && store.deployed.keys().any(|k| is_archive(k)) {
            store.deployed.insert(MODLIST.into(), Some(LOAD_ORDER.into()));
        }
        store.modlist_tracked = true; // saved by deploy(), even if a copy below fails
    }
    switch_game(store, game)?;
    let want = plan(store);
    let keys: HashSet<&str> = want.iter().map(|w| w.0.as_str()).collect();
    let gone: Vec<String> = store.deployed.keys().filter(|k| !keys.contains(k.as_str()) && *k != MODLIST).cloned().collect();
    for key in &gone {
        remove(store, game, key)?;
    }
    let mut changed = gone.len();
    for (key, mid, rel) in &want {
        let (src, dst) = (join(&join(&store.staging, mid), rel), join(game, rel));
        if !store.deployed.contains_key(key) && dst.exists() {
            let bak = join(&store.backups, key);
            if !bak.exists() {
                copy_to(&dst, &bak)?;
            }
        }
        if store.deployed.get(key).and_then(Option::as_ref) != Some(mid) || !dst.exists() {
            copy_to(&src, &dst)?;
            changed += 1;
        }
        store.deployed.insert(key.clone(), Some(mid.clone()));
    }
    let (managed, others) = load_order(store, game);
    if !managed.is_empty() {
        // the user's own modlist.txt is backed up once, like any game file we replace
        let (f, bak) = (join(game, MODLIST), join(&store.backups, MODLIST));
        if !store.deployed.contains_key(MODLIST) && f.exists() && !bak.exists() {
            copy_to(&f, &bak)?;
        }
        // Python's write_text on Windows writes "\r\n" line ends.
        let body: String = managed.iter().chain(&others).map(|n| format!("{n}\r\n")).collect();
        fs::write(&f, body).map_err(|e| err(e, &f))?;
        store.deployed.insert(MODLIST.into(), Some(LOAD_ORDER.into()));
    } else if store.deployed.contains_key(MODLIST) {
        remove(store, game, MODLIST)?; // no managed archive is on: give the folder back (the user's file, or none)
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use testkit::{TempDir, fake_game, mod_rec, stage};

    struct Fx {
        _t: TempDir,
        game: PathBuf,
        store: Store,
    }

    fn setup() -> Fx {
        let t = TempDir::new();
        let game = fake_game(t.path());
        let mut store = Store::open(t.path().join("data")).unwrap();
        store.settings.game_dir = game.to_string_lossy().into_owned();
        Fx { _t: t, game, store }
    }

    fn add(store: &mut Store, id: &str, files: &[&str]) {
        stage(store, mod_rec(id, files), &[]);
    }

    fn off(store: &mut Store, id: &str) {
        store.mod_by_id_mut(id).unwrap().enabled = false;
    }

    fn read(p: &Path) -> String {
        fs::read_to_string(p).unwrap()
    }

    fn words(p: &Path) -> Vec<String> {
        read(p).split_whitespace().map(String::from).collect()
    }

    fn modlist(game: &Path) -> PathBuf {
        game.join(MODLIST)
    }

    /// Reload the store from a state file shaped like v1.0.0's (it has no modlist_tracked key).
    fn as_v100(store: &mut Store) {
        store.save().unwrap();
        let mut d: Value = serde_json::from_str(&read(&store.file)).unwrap();
        d.as_object_mut().unwrap().remove("modlist_tracked");
        fs::write(&store.file, d.to_string()).unwrap();
        let s = Store::open(&store.root).unwrap();
        *store = s;
        assert!(!store.modlist_tracked);
    }

    fn user_modlist(game: &Path) -> PathBuf {
        fs::write(game.join("archive/pc/mod/zz_user.archive"), "x").unwrap();
        let f = modlist(game);
        fs::write(&f, "zz_user.archive\n# mine\n").unwrap();
        f
    }

    #[test]
    fn priority_disable_and_backup() {
        // test_deploy.py Deploy.test_priority_disable_and_backup
        let Fx { _t, game, mut store } = setup();
        fs::write(game.join("bin/x64/global.ini"), "vanilla").unwrap();
        add(&mut store, "a", &["archive/pc/mod/a.archive", "bin/x64/global.ini", "r6/scripts/a/x.reds"]);
        add(&mut store, "b", &["archive/pc/mod/b.archive", "bin/x64/global.ini"]);
        assert_eq!(deploy(&mut store).unwrap(), 4); // 5 file entries, 4 distinct paths
        assert_eq!(deploy(&mut store).unwrap(), 0); // nothing changed -> nothing copied
        assert_eq!(read(&game.join("bin/x64/global.ini")), "b:bin/x64/global.ini"); // lower in list wins
        assert_eq!(words(&modlist(&game)), ["b.archive", "a.archive"]);

        off(&mut store, "b");
        deploy(&mut store).unwrap();
        assert_eq!(read(&game.join("bin/x64/global.ini")), "a:bin/x64/global.ini");
        assert!(!game.join("archive/pc/mod/b.archive").exists());

        off(&mut store, "a");
        deploy(&mut store).unwrap();
        assert_eq!(read(&game.join("bin/x64/global.ini")), "vanilla"); // backup restored
        assert!(!game.join("r6/scripts/a").exists()); // empty mod folder pruned
        assert!(store.deployed.is_empty());
    }

    #[test]
    fn edited_files_are_kept_until_the_mod_changes() {
        // test_deploy.py Deploy.test_edited_files_are_kept_until_the_mod_changes
        let Fx { _t, game, mut store } = setup();
        add(&mut store, "a", &["r6/input/keys.xml"]);
        deploy(&mut store).unwrap();
        fs::write(game.join("r6/input/keys.xml"), "my own keys").unwrap();
        assert_eq!(deploy(&mut store).unwrap(), 0);
        assert_eq!(read(&game.join("r6/input/keys.xml")), "my own keys");
        store.deployed.insert("r6/input/keys.xml".into(), None); // what an update of mod "a" does
        deploy(&mut store).unwrap();
        assert_eq!(read(&game.join("r6/input/keys.xml")), "a:r6/input/keys.xml");
    }

    #[test]
    fn new_game_folder_takes_mods_out_of_the_old_one() {
        // test_deploy.py Deploy.test_new_game_folder_takes_mods_out_of_the_old_one
        let Fx { _t, game, mut store } = setup();
        fs::write(game.join("bin/x64/global.ini"), "vanilla A").unwrap();
        add(&mut store, "a", &["bin/x64/global.ini", "archive/pc/mod/a.archive"]);
        deploy(&mut store).unwrap();
        let t2 = TempDir::new();
        let other = fake_game(t2.path());
        fs::write(other.join("bin/x64/global.ini"), "vanilla B").unwrap();
        store.settings.game_dir = other.to_string_lossy().into_owned();
        deploy(&mut store).unwrap();
        assert_eq!(read(&game.join("bin/x64/global.ini")), "vanilla A"); // old folder restored
        assert!(!game.join("archive/pc/mod/a.archive").exists());
        assert_eq!(read(&other.join("bin/x64/global.ini")), "a:bin/x64/global.ini");
        off(&mut store, "a");
        deploy(&mut store).unwrap();
        assert_eq!(read(&other.join("bin/x64/global.ini")), "vanilla B"); // B's own backup, not A's
    }

    #[test]
    fn state_is_saved_when_a_copy_fails() {
        // test_deploy.py Deploy.test_state_is_saved_when_a_copy_fails
        let Fx { _t, mut store, .. } = setup();
        add(&mut store, "a", &["archive/pc/mod/a.archive", "archive/pc/mod/b.archive"]);
        fs::remove_file(store.staging.join("a/archive/pc/mod/b.archive")).unwrap(); // the second copy will fail
        assert!(deploy(&mut store).is_err());
        let saved = Store::open(&store.root).unwrap();
        let got: Vec<(&String, &Option<String>)> = saved.deployed.iter().collect();
        assert_eq!(got, [(&"archive/pc/mod/a.archive".to_string(), &Some("a".to_string()))]);
    }

    #[test]
    fn users_own_modlist_is_backed_up_and_put_back() {
        // test_deploy.py Deploy.test_users_own_modlist_is_backed_up_and_put_back
        let Fx { _t, game, mut store } = setup();
        let f = modlist(&game);
        fs::write(&f, "my_order.archive\n").unwrap();
        deploy(&mut store).unwrap(); // no managed archive: the user's file is not touched
        assert_eq!(read(&f), "my_order.archive\n");
        add(&mut store, "a", &["archive/pc/mod/a.archive"]);
        deploy(&mut store).unwrap();
        assert_eq!(words(&f), ["a.archive"]);
        off(&mut store, "a");
        deploy(&mut store).unwrap();
        assert_eq!(read(&f), "my_order.archive\n"); // put back
        assert!(store.deployed.is_empty());
        add(&mut store, "b", &["archive/pc/mod/b.archive"]);
        fs::remove_file(&f).unwrap();
        deploy(&mut store).unwrap();
        off(&mut store, "b");
        deploy(&mut store).unwrap();
        assert!(!f.exists()); // ours, so removed when no archive is left
    }

    #[test]
    fn users_modlist_comes_back_with_their_own_archives_present() {
        // test_deploy.py Deploy.test_users_modlist_comes_back_with_their_own_archives_present
        let Fx { _t, game, mut store } = setup();
        let f = modlist(&game);
        fs::write(game.join("archive/pc/mod/zz_user.archive"), "x").unwrap();
        fs::write(&f, "zz_user.archive\n# my notes\n").unwrap();
        deploy(&mut store).unwrap(); // no managed mod yet: hands off
        assert_eq!(read(&f), "zz_user.archive\n# my notes\n");
        add(&mut store, "a", &["archive/pc/mod/a.archive"]);
        deploy(&mut store).unwrap();
        assert_eq!(words(&f), ["a.archive", "zz_user.archive"]);
        off(&mut store, "a");
        deploy(&mut store).unwrap();
        assert_eq!(read(&f), "zz_user.archive\n# my notes\n");
        assert!(!store.deployed.contains_key(MODLIST));
    }

    #[test]
    fn upgrade_from_v100_does_not_keep_the_old_generated_modlist() {
        // test_deploy.py Deploy.test_upgrade_from_v100_does_not_keep_the_old_generated_modlist
        let Fx { _t, game, mut store } = setup();
        add(&mut store, "a", &["archive/pc/mod/a.archive"]);
        deploy(&mut store).unwrap();
        store.deployed.shift_remove(MODLIST); // v1.0.0 did not record the file
        let _ = fs::remove_file(store.backups.join(MODLIST));
        as_v100(&mut store);
        off(&mut store, "a");
        deploy(&mut store).unwrap();
        assert!(!modlist(&game).exists());
        assert!(Store::open(&store.root).unwrap().modlist_tracked); // decided once only
    }

    #[test]
    fn upgrade_that_also_switches_game_folder_cleans_the_old_one() {
        // test_deploy.py Deploy.test_upgrade_that_also_switches_game_folder_cleans_the_old_one
        let Fx { _t, game, mut store } = setup();
        add(&mut store, "a", &["archive/pc/mod/a.archive"]);
        deploy(&mut store).unwrap();
        store.deployed.shift_remove(MODLIST); // v1.0.0 did not record the file it wrote
        as_v100(&mut store);
        let t2 = TempDir::new();
        let other = fake_game(t2.path());
        store.settings.game_dir = other.to_string_lossy().into_owned();
        deploy(&mut store).unwrap();
        assert!(!modlist(&game).exists()); // v1.0.0's list left the old folder
        assert_eq!(words(&modlist(&other)), ["a.archive"]);
    }

    /// An .xl-only mod never makes the user's modlist.txt ours.
    fn s1(upgrade: bool) {
        let Fx { _t, game, mut store } = setup();
        let f = user_modlist(&game);
        add(&mut store, "b", &["archive/pc/mod/b.archive.xl"]);
        deploy(&mut store).unwrap();
        if upgrade {
            as_v100(&mut store); // deployed holds archive/pc/mod/b.archive.xl, which is not an .archive
        }
        deploy(&mut store).unwrap();
        deploy(&mut store).unwrap();
        assert_eq!(read(&f), "zz_user.archive\n# mine\n");
    }

    /// The user's file comes back after an archive mod goes off, and stays.
    fn s2(upgrade: bool) {
        let Fx { _t, game, mut store } = setup();
        let f = user_modlist(&game);
        add(&mut store, "a", &["archive/pc/mod/a.archive"]);
        add(&mut store, "b", &["archive/pc/mod/b.archive.xl"]);
        if upgrade {
            as_v100(&mut store);
        }
        deploy(&mut store).unwrap();
        off(&mut store, "a");
        deploy(&mut store).unwrap();
        deploy(&mut store).unwrap();
        assert_eq!(read(&f), "zz_user.archive\n# mine\n");
    }

    /// A failed first deploy does not lose the user's file.
    fn s3(upgrade: bool) {
        let Fx { _t, game, mut store } = setup();
        let f = user_modlist(&game);
        add(&mut store, "a", &["archive/pc/mod/a.archive", "archive/pc/mod/b.archive"]);
        let b = store.staging.join("a/archive/pc/mod/b.archive");
        fs::remove_file(&b).unwrap();
        if upgrade {
            as_v100(&mut store);
        }
        assert!(deploy(&mut store).is_err());
        assert_eq!(read(&f), "zz_user.archive\n# mine\n");
        fs::write(&b, "b").unwrap();
        deploy(&mut store).unwrap();
        off(&mut store, "a");
        deploy(&mut store).unwrap();
        assert_eq!(read(&f), "zz_user.archive\n# mine\n");
    }

    #[test]
    fn s1_new() {
        // test_deploy.py Deploy.test_s1_new
        s1(false);
    }

    #[test]
    fn s1_upgrade() {
        // test_deploy.py Deploy.test_s1_upgrade
        s1(true);
    }

    #[test]
    fn s2_new() {
        // test_deploy.py Deploy.test_s2_new
        s2(false);
    }

    #[test]
    fn s2_upgrade() {
        // test_deploy.py Deploy.test_s2_upgrade
        s2(true);
    }

    #[test]
    fn s3_new() {
        // test_deploy.py Deploy.test_s3_new
        s3(false);
    }

    #[test]
    fn s3_upgrade() {
        // test_deploy.py Deploy.test_s3_upgrade
        s3(true);
    }

    #[test]
    fn mod_shipping_a_modlist_is_ignored() {
        // test_deploy.py Deploy.test_mod_shipping_a_modlist_is_ignored
        let Fx { _t, game, mut store } = setup();
        add(&mut store, "a", &["archive/pc/mod/a.archive", "archive/pc/mod/modlist.txt"]);
        deploy(&mut store).unwrap();
        assert_eq!(deploy(&mut store).unwrap(), 0); // stable: no change reported forever
        assert_eq!(words(&modlist(&game)), ["a.archive"]);
    }

    #[test]
    fn unmanaged_archives_kept_last() {
        // test_deploy.py Deploy.test_unmanaged_archives_kept_last
        let Fx { _t, game, mut store } = setup();
        fs::write(game.join("archive/pc/mod/zzz_manual.archive"), "x").unwrap();
        add(&mut store, "a", &["archive/pc/mod/a.archive"]);
        deploy(&mut store).unwrap();
        assert_eq!(words(&modlist(&game)), ["a.archive", "zzz_manual.archive"]);
    }

    #[test]
    fn needs_game_folder() {
        // test_deploy.py Deploy.test_needs_game_folder
        let Fx { _t, mut store, .. } = setup();
        store.settings.game_dir = String::new();
        assert!(deploy(&mut store).is_err());
    }
}
