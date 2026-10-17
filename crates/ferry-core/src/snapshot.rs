//! Spec 4.4: a pair's destination snapshot, what its destination root held when Ferry last saw it there.
//!
//! Text: a header line, then one block per directory, a `D <rel dir>` line followed by the
//! directory's entries in `name_order` (`F <size> <mtime> <name>` for a file, `S <name>` for a
//! subdirectory), then `E`. Blocks may come in any order; `open` indexes where each starts,
//! so a reader holds one directory in memory at a time.

use crate::cargo::write_atomic_with;
use crate::diff::{at, join};
use crate::entry::{list_dir, name_order, Entry};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const HEADER: &str = "ferry-snapshot 1";

pub struct Snapshot;

/// Reads a snapshot one directory at a time. The default reader is the empty snapshot.
#[derive(Default)]
pub struct SnapshotReader {
    file: Option<BufReader<File>>,
    /// Lowercase rel dir → offset of its `D` line.
    index: HashMap<String, u64>,
}

impl Snapshot {
    /// Walk `destination_root` and write it to `out`. The old snapshot is replaced only by a complete
    /// walk: a folder that can't be listed fails the whole record.
    pub fn record(destination_root: &Path, out: &Path) -> io::Result<()> {
        if let Some(d) = out.parent() {
            fs::create_dir_all(d)?;
        }
        write_atomic_with(out, |w| {
            writeln!(w, "{HEADER}")?;
            let mut stack = vec![String::new()];
            while let Some(dir) = stack.pop() {
                let entries = list_dir(&at(destination_root, &dir))?;
                write_block(w, &dir, &entries)?;
                stack.extend(entries.iter().filter(|e| e.is_dir).map(|e| join(&dir, &e.name)));
            }
            writeln!(w, "E")
        })?;
        fs::write(marker(out), b"")
    }

    /// A missing snapshot is `NotFound`; a damaged one is `InvalidData`.
    pub fn open(path: &Path) -> io::Result<SnapshotReader> {
        let mut r = BufReader::new(File::open(path)?);
        let pos = header(&mut r)?;
        let mut index = HashMap::new();
        blocks(&mut r, pos, |at, dir, _| match index.insert(dir.to_lowercase(), at) {
            None => Ok(true),
            Some(_) => Err(damaged()),
        })?;
        if !index.contains_key("") {
            return Err(damaged());
        }
        Ok(SnapshotReader { file: Some(r), index })
    }

    /// Whether a snapshot was ever recorded at `path` (it may since be damaged or gone).
    pub fn recorded_before(path: &Path) -> bool {
        marker(path).exists()
    }

    /// Changes verified at the destination, in order, in one rewrite. Without a snapshot this does nothing.
    pub fn apply(path: &Path, changes: &[Change]) -> io::Result<()> {
        rewrite(path, &edits(changes))
    }

    /// A verified push landed at the destination. Without a snapshot this does nothing.
    pub fn apply_push(path: &Path, rel: &str, size: u64, mtime: i64) -> io::Result<()> {
        Self::apply(path, &[Change::Push { rel: rel.into(), size, mtime }])
    }

    /// A delete was applied at the destination. Without a snapshot this does nothing.
    pub fn apply_delete(path: &Path, rel: &str) -> io::Result<()> {
        Self::apply(path, &[Change::Delete { rel: rel.into() }])
    }
}

/// One change verified at the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Push { rel: String, size: u64, mtime: i64 },
    Delete { rel: String },
}

/// `<snapshot>.recorded`: left by the first successful record, so a snapshot that later goes
/// missing is not mistaken for a first trip.
fn marker(path: &Path) -> PathBuf {
    let mut m = path.as_os_str().to_owned();
    m.push(".recorded");
    m.into()
}

fn edits(changes: &[Change]) -> Vec<Edit> {
    let mut out = vec![];
    for c in changes {
        match c {
            Change::Push { rel, size, mtime } => {
                let parts: Vec<&str> = rel.split('\\').collect();
                for (i, name) in parts.iter().enumerate() {
                    let file = i + 1 == parts.len();
                    let (size, mtime) = if file { (*size, *mtime) } else { (0, 0) };
                    let to = Entry { name: name.to_string(), is_dir: !file, size, mtime };
                    out.push(Edit { dir: parts[..i].join("\\"), name: name.to_string(), to: Some(to) });
                }
            }
            Change::Delete { rel } => {
                let (dir, name) = rel.rsplit_once('\\').unwrap_or(("", rel));
                out.push(Edit { dir: dir.into(), name: name.into(), to: None });
            }
        }
    }
    out
}

impl SnapshotReader {
    /// One directory's entries in `name_order`; a directory the snapshot lacks is empty.
    pub fn dir(&mut self, rel_dir: &str) -> io::Result<Vec<Entry>> {
        let (Some(r), Some(&at)) = (self.file.as_mut(), self.index.get(&rel_dir.to_lowercase())) else {
            return Ok(vec![]);
        };
        r.seek(SeekFrom::Start(at))?;
        let mut out = vec![];
        blocks(r, at, |_, _, v| {
            out = v;
            Ok(false)
        })?;
        out.sort_by(|a, b| name_order(&a.name, &b.name));
        Ok(out)
    }
}

fn damaged() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "the snapshot file is damaged")
}

/// Check the header line; the offset after it.
fn header(r: &mut impl BufRead) -> io::Result<u64> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if line.strip_suffix('\n') != Some(HEADER) {
        return Err(damaged());
    }
    Ok(n as u64)
}

fn parse_entry(l: &str) -> Option<Entry> {
    if let Some(name) = l.strip_prefix("S ") {
        return (!name.is_empty()).then(|| Entry { name: name.into(), is_dir: true, size: 0, mtime: 0 });
    }
    let mut f = l.strip_prefix("F ")?.splitn(3, ' ');
    let (size, mtime, name) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()?);
    (!name.is_empty()).then(|| Entry { name: name.into(), is_dir: false, size, mtime })
}

fn write_block(w: &mut dyn Write, dir: &str, entries: &[Entry]) -> io::Result<()> {
    // A case-sensitive share can hold both; `open` indexes folders ignoring case, so refuse
    // rather than write a snapshot it would reject. Files differing in case are kept.
    if let Some(p) = entries.windows(2).find(|p| p[0].is_dir && p[1].is_dir && p[0].name.to_lowercase() == p[1].name.to_lowercase()) {
        let msg = format!("folders {} and {} differ only in case", join(dir, &p[0].name), join(dir, &p[1].name));
        return Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
    }
    writeln!(w, "D {dir}")?;
    for e in entries {
        if e.name.contains(['\n', '\r']) {
            let msg = format!("{}: a line break in the name", join(dir, &e.name));
            return Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
        }
        if e.is_dir {
            writeln!(w, "S {}", e.name)?;
        } else {
            writeln!(w, "F {} {} {}", e.size, e.mtime, e.name)?;
        }
    }
    Ok(())
}

/// Walk the blocks from `pos`, the offset of a `D` line. `each(offset of its D line, dir,
/// entries)` says whether to go on; a walk that isn't stopped must end at `E`.
fn blocks(
    r: &mut impl BufRead,
    mut pos: u64,
    mut each: impl FnMut(u64, String, Vec<Entry>) -> io::Result<bool>,
) -> io::Result<()> {
    let mut line = String::new();
    let mut cur: Option<(u64, String, Vec<Entry>)> = None;
    loop {
        line.clear();
        let n = r.read_line(&mut line)? as u64;
        let l = line.strip_suffix('\n').ok_or_else(damaged)?;
        let end = l == "E";
        if end || l.starts_with("D ") {
            if let Some((at, dir, v)) = cur.take() {
                if !each(at, dir, v)? {
                    return Ok(());
                }
            }
            if end {
                return Ok(());
            }
            cur = Some((pos, l[2..].to_string(), vec![]));
        } else {
            cur.as_mut().ok_or_else(damaged)?.2.push(parse_entry(l).ok_or_else(damaged)?);
        }
        pos += n;
    }
}

/// Put `to` (or nothing) in place of the entry `name` of directory `dir`.
struct Edit {
    dir: String,
    name: String,
    to: Option<Entry>,
}

impl Edit {
    fn apply(&self, v: &mut Vec<Entry>) {
        let i = v.iter().position(|e| e.name.to_lowercase() == self.name.to_lowercase());
        match (&self.to, i) {
            // The folder is already there.
            (Some(e), Some(i)) if e.is_dir && v[i].is_dir => {}
            (Some(e), Some(i)) => v[i] = e.clone(),
            (Some(e), None) => {
                v.push(e.clone());
                v.sort_by(|a, b| name_order(&a.name, &b.name));
            }
            (None, Some(i)) => {
                v.remove(i);
            }
            (None, None) => {}
        }
    }
}

/// Copy the snapshot block by block with `edits` made in order; one block per directory it
/// lacks is added at the end. A missing snapshot stays missing.
// ponytail: each call rewrites the whole file; callers batch changes. A journal if that gets slow.
fn rewrite(path: &Path, edits: &[Edit]) -> io::Result<()> {
    let mut src = match File::open(path) {
        Ok(f) => BufReader::new(f),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let pos = header(&mut src)?;
    write_atomic_with(path, move |w| {
        writeln!(w, "{HEADER}")?;
        let mut seen = vec![false; edits.len()];
        blocks(&mut src, pos, |_, dir, mut v| {
            for (e, s) in edits.iter().zip(&mut seen) {
                if e.dir.to_lowercase() == dir.to_lowercase() {
                    *s = true;
                    e.apply(&mut v);
                }
            }
            write_block(w, &dir, &v)?;
            Ok(true)
        })?;
        drop(src); // closed before the rename
        // New directories, grouped ignoring case: two `D` blocks for one folder would be damage.
        let mut new: Vec<(String, Vec<Entry>)> = vec![];
        for (e, _) in edits.iter().zip(&seen).filter(|(_, s)| !**s) {
            let i = match new.iter().position(|(d, _)| d.to_lowercase() == e.dir.to_lowercase()) {
                Some(i) => i,
                None if e.to.is_some() => {
                    new.push((e.dir.clone(), vec![]));
                    new.len() - 1
                }
                None => continue,
            };
            e.apply(&mut new[i].1);
        }
        for (dir, v) in &new {
            write_block(w, dir, v)?;
        }
        writeln!(w, "E")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::list_dir;
    use std::fs;

    fn put(root: &Path, rel: &str, size: usize) {
        let p = rel.split('\\').fold(root.to_path_buf(), |a, c| a.join(c));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![b'x'; size]).unwrap();
    }

    fn names(v: &[Entry]) -> Vec<(&str, bool, u64)> {
        v.iter().map(|e| (e.name.as_str(), e.is_dir, e.size)).collect()
    }

    /// A destination tree with a nested folder, an empty folder and a name with spaces.
    fn tree() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let destination = d.path().join("home");
        put(&destination, "b.zip", 2);
        put(&destination, "A.zip", 1);
        put(&destination, " lead space.txt", 4);
        put(&destination, "sub\\c.zip", 3);
        put(&destination, "sub\\deep\\d.zip", 5);
        fs::create_dir_all(destination.join("empty")).unwrap();
        let snap = d.path().join("snapshots").join("p.snap");
        (d, snap)
    }

    #[test]
    fn snapshot_round_trip_by_directory() {
        let (d, snap) = tree();
        let destination = d.path().join("home");
        Snapshot::record(&destination, &snap).unwrap();
        assert!(!snap.with_extension("snap.tmp").exists());
        let mut r = Snapshot::open(&snap).unwrap();
        // Same entries, order, sizes and mtimes as a live listing (directory mtimes aside).
        for dir in ["", "sub", "sub\\deep", "empty"] {
            let live = list_dir(&if dir.is_empty() { destination.clone() } else { destination.join(dir) }).unwrap();
            let got = r.dir(dir).unwrap();
            assert_eq!(names(&got), names(&live), "dir {dir:?}");
            for (g, l) in got.iter().zip(&live).filter(|(g, _)| !g.is_dir) {
                assert_eq!(g.mtime, l.mtime, "{}", g.name);
            }
        }
        assert_eq!(names(&r.dir("").unwrap()), [(" lead space.txt", false, 4), ("A.zip", false, 1), ("b.zip", false, 2), ("empty", true, 0), ("sub", true, 0)]);
        assert_eq!(names(&r.dir("SUB").unwrap()), [("c.zip", false, 3), ("deep", true, 0)], "directories match ignoring case");
        assert!(r.dir("nope").unwrap().is_empty());
        // Directories can be read in any order, and again.
        assert_eq!(names(&r.dir("sub\\deep").unwrap()), [("d.zip", false, 5)]);
        assert_eq!(r.dir("").unwrap().len(), 5);
        assert!(SnapshotReader::default().dir("").unwrap().is_empty(), "the empty snapshot");
    }

    #[test]
    fn record_replaces_the_old_snapshot() {
        let (d, snap) = tree();
        let destination = d.path().join("home");
        Snapshot::record(&destination, &snap).unwrap();
        fs::remove_dir_all(destination.join("sub")).unwrap();
        Snapshot::record(&destination, &snap).unwrap();
        let mut r = Snapshot::open(&snap).unwrap();
        assert!(!r.dir("").unwrap().iter().any(|e| e.name == "sub"));
        assert!(r.dir("sub").unwrap().is_empty());
    }

    #[test]
    fn unlistable_destination_root_keeps_the_old_snapshot() {
        let (d, snap) = tree();
        Snapshot::record(&d.path().join("home"), &snap).unwrap();
        let before = fs::read(&snap).unwrap();
        assert!(Snapshot::record(&d.path().join("missing"), &snap).is_err());
        assert_eq!(fs::read(&snap).unwrap(), before);
    }

    #[test]
    fn pushes_and_deletes_update_the_snapshot() {
        let (d, snap) = tree();
        Snapshot::record(&d.path().join("home"), &snap).unwrap();
        Snapshot::apply_push(&snap, "a0.zip", 9, 77).unwrap();
        Snapshot::apply_push(&snap, "b.zip", 20, 88).unwrap(); // replaces
        Snapshot::apply_push(&snap, "new\\dir\\n.zip", 1, 5).unwrap(); // new folders
        Snapshot::apply_push(&snap, "SUB\\c2.zip", 6, 6).unwrap(); // existing folder, other case
        Snapshot::apply_delete(&snap, "A.zip").unwrap();
        Snapshot::apply_delete(&snap, "sub\\deep\\D.ZIP").unwrap();
        Snapshot::apply_delete(&snap, "not\\there.zip").unwrap();
        let mut r = Snapshot::open(&snap).unwrap();
        let root = r.dir("").unwrap();
        assert_eq!(names(&root), [(" lead space.txt", false, 4), ("a0.zip", false, 9), ("b.zip", false, 20), ("empty", true, 0), ("new", true, 0), ("sub", true, 0)]);
        assert_eq!(root[2].mtime, 88);
        assert_eq!(names(&r.dir("new").unwrap()), [("dir", true, 0)]);
        assert_eq!(names(&r.dir("new\\dir").unwrap()), [("n.zip", false, 1)]);
        assert_eq!(names(&r.dir("sub").unwrap()), [("c.zip", false, 3), ("c2.zip", false, 6), ("deep", true, 0)]);
        assert!(r.dir("sub\\deep").unwrap().is_empty());
        drop(r);
        // No snapshot stays no snapshot: an update never invents one.
        let none = d.path().join("snapshots").join("q.snap");
        Snapshot::apply_push(&none, "a.zip", 1, 1).unwrap();
        Snapshot::apply_delete(&none, "a.zip").unwrap();
        assert!(!none.exists());
    }

    #[test]
    fn one_batch_into_a_new_folder_gives_one_block() {
        let (d, snap) = tree();
        Snapshot::record(&d.path().join("home"), &snap).unwrap();
        let push = |rel: &str| Change::Push { rel: rel.into(), size: 1, mtime: 1 };
        let batch = [push("new\\a.zip"), push("NEW\\b.zip"), push("new\\deep\\c.zip"), Change::Delete { rel: "new\\a.zip".into() }];
        Snapshot::apply(&snap, &batch).unwrap();
        let mut r = Snapshot::open(&snap).expect("a folder written twice would be rejected");
        assert_eq!(names(&r.dir("new").unwrap()), [("b.zip", false, 1), ("deep", true, 0)]);
        assert_eq!(names(&r.dir("new\\deep").unwrap()), [("c.zip", false, 1)]);
    }

    #[test]
    fn recorded_before_survives_a_lost_snapshot() {
        let (d, snap) = tree();
        assert!(!Snapshot::recorded_before(&snap));
        assert!(Snapshot::record(&d.path().join("missing"), &snap).is_err());
        assert!(!Snapshot::recorded_before(&snap), "only a successful record counts");
        Snapshot::record(&d.path().join("home"), &snap).unwrap();
        fs::remove_file(&snap).unwrap();
        assert!(Snapshot::recorded_before(&snap));
    }

    #[test]
    fn names_differing_only_in_case() {
        // Files: kept, and the snapshot opens.
        let d = tempfile::tempdir().unwrap();
        let snap = d.path().join("p.snap");
        fs::write(&snap, "ferry-snapshot 1\nD \nF 1 1 A.zip\nF 2 2 a.zip\nE\n").unwrap();
        assert_eq!(names(&Snapshot::open(&snap).unwrap().dir("").unwrap()), [("A.zip", false, 1), ("a.zip", false, 2)]);
        // Folders: refused with a clear error instead of a snapshot `open` would reject.
        let dir = |n: &str| Entry { name: n.into(), is_dir: true, size: 0, mtime: 0 };
        let e = write_block(&mut Vec::new(), "x", &[dir("Sub"), dir("sub")]).unwrap_err();
        assert_eq!(e.to_string(), "folders x\\Sub and x\\sub differ only in case");
        write_block(&mut Vec::new(), "x", &[dir("Sub"), Entry { is_dir: false, ..dir("sub") }]).unwrap();
    }

    #[test]
    fn damaged_snapshot_is_invalid_data() {
        let (d, snap) = tree();
        Snapshot::record(&d.path().join("home"), &snap).unwrap();
        let good = fs::read(&snap).unwrap();
        let cut = &good[..good.len() - 3]; // torn: no end marker
        for bad in [cut, b"garbage", b"", &[0xff, 0xfe, 0x00][..]] {
            fs::write(&snap, bad).unwrap();
            let e = Snapshot::open(&snap).err().expect("damaged snapshot opened");
            assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        }
        assert_eq!(Snapshot::open(&d.path().join("none.snap")).err().unwrap().kind(), io::ErrorKind::NotFound);
    }
}
