//! A backup of Ferry's settings and the laptop's state in one JSON file: `config.toml`, the cargo
//! list, the delete queue and the destination snapshots. No passwords: those live in Windows
//! Credential Manager and are entered again after a restore.

use crate::cargo::{write_atomic, CargoManifest};
use crate::config::Config;
use crate::deletes::DeleteQueue;
use crate::entry::safe_name;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

const FORMAT: &str = "ferry-backup 1";

#[derive(Debug, Serialize, Deserialize)]
struct Backup {
    format: String,
    /// The Ferry version that wrote it, for a person reading the file.
    version: String,
    /// `config.toml` as it was on disk.
    config: String,
    manifest: CargoManifest,
    deletes: DeleteQueue,
    /// File name in the snapshots folder (`<pair>.snap`, `<pair>.snap.recorded`) → its text.
    snapshots: BTreeMap<String, String>,
    /// `identity.txt` (beside the cargo list): the tokens left on each server, so a restored
    /// laptop still recognises them.
    #[serde(default)]
    identity: String,
}

/// Where the files a backup holds live.
pub struct Files<'a> {
    pub config: &'a Path,
    pub manifest: &'a Path,
    pub deletes: &'a Path,
    pub snapshots: &'a Path,
}

/// What a restore kept.
#[derive(Debug, PartialEq)]
pub struct Restored {
    /// Carried files still in the cargo folder, which the restored cargo list keeps.
    pub kept: usize,
    /// Carried files no longer on the laptop, dropped from the list (carried again later).
    pub dropped: usize,
}

/// The backup file's text.
pub fn export(f: &Files, version: &str) -> io::Result<String> {
    let config = fs::read_to_string(f.config)?;
    let identity = fs::read_to_string(identity_path(f)).unwrap_or_default();
    let mut snapshots = BTreeMap::new();
    for e in fs::read_dir(f.snapshots).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".snap") || name.ends_with(".snap.recorded") {
            snapshots.insert(name, fs::read_to_string(e.path())?);
        }
    }
    let b = Backup {
        format: FORMAT.into(),
        version: version.into(),
        config,
        manifest: CargoManifest::load(f.manifest)?,
        deletes: DeleteQueue::load(f.deletes)?,
        snapshots,
        identity,
    };
    serde_json::to_string_pretty(&b).map_err(io::Error::other)
}

/// Replace the settings and state with the backup's. Refused while the laptop carries files or
/// holds deletes, which the restore would orphan (or when it can't tell); `dest` names the
/// destination in that message. Carried files are kept only if they are still in the cargo folder
/// (`default_cargo` unless the backup names another), and each one dropped holds its folder's
/// deletes until it is carried again. Nothing is written unless the whole backup checks out.
pub fn restore(text: &str, f: &Files, default_cargo: &Path, dest: &str) -> Result<Restored, String> {
    let busy = |m: &CargoManifest, q: &DeleteQueue| !m.items.is_empty() || m.partial.is_some() || q.pairs.values().any(|p| !p.paths.is_empty());
    let unreadable = |e: io::Error| format!("Ferry can't read what the laptop is carrying ({e}), so it won't restore over it. Let one trip finish, then restore.");
    let (m, q) = (CargoManifest::load(f.manifest).map_err(unreadable)?, DeleteQueue::load(f.deletes).map_err(unreadable)?);
    if busy(&m, &q) {
        return Err(format!("The laptop is carrying files or holding deletes for {dest}. Unload at {dest} first, then restore."));
    }

    let mut b: Backup = serde_json::from_str(text).map_err(|_| "This isn't a Ferry backup file.".to_string())?;
    if b.format != FORMAT {
        return Err(format!("This backup is from a newer Ferry ({}). Update Ferry, then restore.", b.version));
    }
    let cfg: Config = toml::from_str(&b.config).map_err(|e| format!("The settings in this backup can't be read: {e}"))?;
    // The backup is a file from outside: every name that becomes a path must stay inside its folder.
    let ids: Vec<&str> = cfg.pairs.iter().map(|p| p.id.as_str()).collect();
    // Snapshots of folders no longer in the list are left out, as the engine would drop them.
    b.snapshots.retain(|name, _| name.strip_suffix(".snap").or_else(|| name.strip_suffix(".snap.recorded")).is_some_and(|id| ids.contains(&id)));
    let bad = ids.iter().any(|id| !safe_name(id))
        || b.manifest.items.iter().any(|i| !safe_name(&i.pair) || !rel_ok(&i.rel))
        || b.deletes.pairs.iter().any(|(id, p)| !safe_name(id) || !p.paths.iter().all(|r| rel_ok(r)))
        || b.snapshots.iter().any(|(name, text)| name.ends_with(".snap") && !snapshot_ok(text))
        || !identity_ok(&b.identity);
    if bad {
        return Err("This backup is damaged: it names files outside Ferry's folders.".into());
    }

    let cargo = cfg.options.cargo_dir.clone().unwrap_or_else(|| default_cargo.to_path_buf());
    let mut manifest = b.manifest;
    manifest.partial = None;
    let before = manifest.items.len();
    manifest.items.retain(|i| {
        let p = i.rel.split(['\\', '/']).fold(cargo.join(&i.pair), |a, c| a.join(c));
        let here = fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() == i.size);
        // Never delivered now: like a lost laptop copy, it holds its folder's deletes (the
        // destination keeps the old file until the new one arrives).
        if let (false, Some(q)) = (here, b.deletes.pairs.get_mut(&i.pair)) {
            q.outstanding += 1;
        }
        here
    });
    let kept = manifest.items.len();

    let io = |e: io::Error| format!("Couldn't restore: {e}");
    fs::create_dir_all(f.snapshots).map_err(io)?;
    for e in fs::read_dir(f.snapshots).map_err(io)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".snap") || name.ends_with(".snap.recorded") {
            fs::remove_file(e.path()).map_err(io)?;
        }
    }
    for (name, text) in &b.snapshots {
        write_atomic(&f.snapshots.join(name), text.as_bytes()).map_err(io)?;
    }
    manifest.save(f.manifest).map_err(io)?;
    b.deletes.save(f.deletes).map_err(io)?;
    write_atomic(&identity_path(f), b.identity.as_bytes()).map_err(io)?;
    if let Some(d) = f.config.parent() {
        fs::create_dir_all(d).map_err(io)?;
    }
    // Last, so a failure above leaves the settings as they were.
    write_atomic(f.config, b.config.as_bytes()).map_err(io)?;
    Ok(Restored { kept, dropped: before - kept })
}

fn identity_path(f: &Files) -> std::path::PathBuf {
    f.manifest.with_file_name("identity.txt")
}

/// `source <hex>` / `destination <hex>` lines, nothing else.
fn identity_ok(text: &str) -> bool {
    text.lines().all(|l| match l.split_once(' ') {
        Some(("source" | "destination", t)) => t.len() <= 64 && t.bytes().all(|b| b.is_ascii_hexdigit()),
        _ => l.trim().is_empty(),
    })
}

/// A relative path under a folder: one or more safe names.
fn rel_ok(rel: &str) -> bool {
    rel.split(['\\', '/']).all(safe_name)
}

/// Every directory and name in a snapshot's text stays inside its folder.
fn snapshot_ok(text: &str) -> bool {
    let mut lines = text.lines();
    lines.next().is_some_and(|h| h.starts_with("ferry-snapshot "))
        && lines.all(|l| match l.split_once(' ') {
            None => l == "E",
            Some(("D", d)) => d.is_empty() || rel_ok(d),
            Some(("S", n)) => safe_name(n),
            Some(("F", rest)) => rest.splitn(3, ' ').nth(2).is_some_and(safe_name),
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::{CargoItem, Partial};
    use crate::deletes::PairDeletes;

    struct Env {
        _d: tempfile::TempDir,
        root: std::path::PathBuf,
    }

    impl Env {
        fn new() -> Self {
            let d = tempfile::tempdir().unwrap();
            let root = d.path().to_path_buf();
            Env { _d: d, root }
        }
        fn files(&self) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
            let r = &self.root;
            (r.join("cfg").join("config.toml"), r.join("manifest.json"), r.join("deletes.json"), r.join("snapshots"))
        }
    }

    fn item(rel: &str, size: u64) -> CargoItem {
        CargoItem { pair: "docs".into(), rel: rel.into(), size, mtime: 1, hash: 7 }
    }

    const CONFIG: &str = "[[pairs]]\nid = \"docs\"\nsource_path = \"share/docs\"\ndestination_path = \"share/docs\"\n";
    const SNAP: &str = "ferry-snapshot 1\nD \nF 3 1 a.txt\nS sub\nE\nD sub\nE\n";

    /// A laptop with settings, two carried files (one still in cargo), a delete queue and a snapshot.
    fn source() -> (Env, String) {
        let e = Env::new();
        let (c, m, d, s) = e.files();
        fs::create_dir_all(c.parent().unwrap()).unwrap();
        fs::write(&c, CONFIG).unwrap();
        CargoManifest { items: vec![item("sub\\kept.bin", 4), item("gone.bin", 9)], partial: None }.save(&m).unwrap();
        let mut q = DeleteQueue::default();
        q.pairs.insert("docs".into(), PairDeletes { paths: vec!["old.txt".into()], ..Default::default() });
        q.save(&d).unwrap();
        fs::create_dir_all(&s).unwrap();
        fs::write(s.join("docs.snap"), SNAP).unwrap();
        fs::write(s.join("docs.snap.recorded"), "").unwrap();
        fs::write(s.join("docs.snap.tmp"), "junk").unwrap();
        fs::write(m.with_file_name("identity.txt"), "source 00ff\ndestination \n").unwrap();
        let text = export(&Files { config: &c, manifest: &m, deletes: &d, snapshots: &s }, "0.2.4").unwrap();
        (e, text)
    }

    #[test]
    fn round_trip_keeps_what_is_still_in_cargo() {
        let (_src, text) = source();
        assert!(!text.contains("docs.snap.tmp"));
        let to = Env::new();
        let (c, m, d, s) = to.files();
        let cargo = to.root.join("cargo");
        fs::create_dir_all(cargo.join("docs").join("sub")).unwrap();
        fs::write(cargo.join("docs").join("sub").join("kept.bin"), b"1234").unwrap();
        fs::create_dir_all(&s).unwrap();
        fs::write(s.join("stale.snap"), "x").unwrap();

        let r = restore(&text, &Files { config: &c, manifest: &m, deletes: &d, snapshots: &s }, &cargo, "Home").unwrap();
        assert_eq!(r, Restored { kept: 1, dropped: 1 });
        assert_eq!(DeleteQueue::load(&d).unwrap().pairs["docs"].outstanding, 1, "the dropped file holds the deletes");
        assert_eq!(fs::read_to_string(&c).unwrap(), CONFIG);
        assert_eq!(CargoManifest::load(&m).unwrap().items, vec![item("sub\\kept.bin", 4)]);
        assert_eq!(DeleteQueue::load(&d).unwrap().pairs["docs"].paths, ["old.txt"]);
        assert_eq!(fs::read_to_string(s.join("docs.snap")).unwrap(), SNAP);
        assert!(s.join("docs.snap.recorded").exists() && !s.join("stale.snap").exists());
        assert_eq!(fs::read_to_string(m.with_file_name("identity.txt")).unwrap(), "source 00ff\ndestination \n");
    }

    #[test]
    fn refused_while_carrying() {
        let (_src, text) = source();
        let to = Env::new();
        let (c, m, d, s) = to.files();
        let p = Partial { pair: "docs".into(), rel: "a".into(), size: 1, mtime: 1 };
        CargoManifest { items: vec![], partial: Some(p) }.save(&m).unwrap();
        let e = restore(&text, &Files { config: &c, manifest: &m, deletes: &d, snapshots: &s }, &to.root, "Home").unwrap_err();
        assert!(e.contains("Unload at Home first"), "{e}");
        assert!(!c.exists());
    }

    #[test]
    fn refused_when_the_cargo_list_is_unreadable() {
        let (_src, text) = source();
        let to = Env::new();
        let (c, m, d, s) = to.files();
        fs::write(&m, "not json").unwrap();
        let e = restore(&text, &Files { config: &c, manifest: &m, deletes: &d, snapshots: &s }, &to.root, "Home").unwrap_err();
        assert!(e.contains("can't read what the laptop is carrying"), "{e}");
        assert!(!c.exists());
    }

    #[test]
    fn names_outside_the_folders_are_refused() {
        let (_src, text) = source();
        for (from, to) in [("sub\\\\kept.bin", "..\\\\kept.bin"), ("old.txt", "C:\\\\x"), ("F 3 1 a.txt", "F 3 1 ..")] {
            let bad = text.replace(from, to);
            assert_ne!(bad, text, "{from}");
            let e = Env::new();
            let (c, m, d, s) = e.files();
            let r = restore(&bad, &Files { config: &c, manifest: &m, deletes: &d, snapshots: &s }, &e.root, "Home");
            assert!(r.unwrap_err().contains("damaged"), "{to}");
            assert!(!c.exists() && !m.exists());
        }
    }

    #[test]
    fn other_files_and_newer_formats_are_refused() {
        let (_src, text) = source();
        let e = Env::new();
        let (c, m, d, s) = e.files();
        let f = Files { config: &c, manifest: &m, deletes: &d, snapshots: &s };
        assert!(restore("{}", &f, &e.root, "Home").unwrap_err().contains("isn't a Ferry backup"));
        assert!(restore(&text.replace(FORMAT, "ferry-backup 2"), &f, &e.root, "Home").unwrap_err().contains("newer Ferry"));
    }
}
