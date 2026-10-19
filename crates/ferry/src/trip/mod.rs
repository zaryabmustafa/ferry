pub mod fill;
pub mod identity;
pub mod job;
pub mod unload;

use ferry_core::cargo::{load_or_reset, CargoManifest};
use ferry_core::config::Options;
use ferry_core::deletes::DeleteQueue;
use ferry_core::entry::long_path;
use ferry_core::snapshot::{Change, Snapshot, SnapshotReader};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// One folder pair's roots. `source`/`destination` are LAN roots, the only paths ever passed to
/// `copy_hashed`; `source_meta`/`destination_meta` are remote-address roots, used only for listing and
/// deletes (`None` when unreachable or no remote address is set).
#[derive(Debug, Clone)]
pub struct PairRoots {
    pub id: String,
    pub source: PathBuf,
    pub source_meta: Option<PathBuf>,
    pub destination: PathBuf,
    pub destination_meta: Option<PathBuf>,
    pub allow_empty: bool,
}

pub struct TripCtx<'a> {
    /// Priority order.
    pub pairs: &'a [PairRoots],
    pub cargo_dir: &'a Path,
    pub manifest_path: &'a Path,
    pub deletes_path: &'a Path,
    /// Destination snapshots, `<pair id>.snap` (spec 4.4).
    pub snapshots: &'a Path,
    /// The source has a remote address. Without one, an unload applies the queued deletes as
    /// the source built them; with one that can't be reached, it holds them.
    pub source_remote_configured: bool,
    /// The destination has a remote address: a fill lists it through `destination_meta` (and skips the pair
    /// when it can't), never the snapshot. Without one it diffs against the snapshot.
    pub destination_remote_configured: bool,
    /// What the user calls each side (`config::display_name`), for messages.
    pub source_name: &'a str,
    pub destination_name: &'a str,
    pub opts: &'a Options,
    pub disk_free: &'a dyn Fn() -> u64,
    /// FILETIME ticks.
    pub now: i64,
    pub cancel: &'a AtomicBool,
    /// Spec 4.3: the bulk path is still allowed. Called before each file and on copy progress;
    /// it throttles itself, so it may be called often.
    pub path_ok: &'a dyn Fn() -> bool,
}

impl TripCtx<'_> {
    /// False once the trip should stop. A lost bulk path cancels it, like leaving the place:
    /// a fill keeps its part and `partial` to resume later.
    fn go_on(&self) -> bool {
        if !self.cancel.load(Ordering::Relaxed) && !(self.path_ok)() {
            self.cancel.store(true, Ordering::Relaxed);
        }
        !self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Progress { pair: String, rel: String, done: u64, total: u64 },
    Carried { pair: String, rel: String, size: u64 },
    Deleted { pair: String, rel: String },
    Oversize { pair: String, rel: String, size: u64 },
    HeldForConfirm { pair: String, count: usize },
    Error { pair: String, msg: String },
    Pushed { pair: String, rel: String, size: u64 },
    VerifyFailed { pair: String, rel: String },
    /// A line for Activity only.
    Info { pair: String, msg: String },
    /// A corrupt state file was moved to `aside` and replaced by an empty one.
    StateReset { manifest: bool, aside: PathBuf },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FillReport {
    pub carried: u64,
    pub carried_bytes: u64,
    pub deleted: u64,
    /// Bytes of candidates still not on the laptop after this trip.
    pub outstanding_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnloadReport {
    pub pushed: u64,
    pub pushed_bytes: u64,
    pub discarded: u64,
    pub failed: u64,
    pub deleted: u64,
    /// Pairs whose destination snapshot was recorded for the first time.
    pub first_records: u64,
}

fn part_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".ferry-part");
    s.into()
}

/// Remove a file; already gone counts as removed.
fn remove(p: &Path) -> io::Result<()> {
    match fs::remove_file(long_path(p)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Remove `file`'s parent folders up to (not including) `root` while they are empty; best effort.
fn prune_dirs(root: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir.filter(|d| *d != root && d.starts_with(root)) {
        if fs::remove_dir(long_path(d)).is_err() {
            return;
        }
        dir = d.parent();
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The manifest for a trip. A corrupt one is moved aside, reported, and replaced by an empty
/// one; the files it listed stay, and the next fill's copy replaces them.
fn load_manifest(path: &Path, emit: &mut dyn FnMut(Event)) -> io::Result<CargoManifest> {
    let (m, aside) = load_or_reset(path, unix_now())?;
    if let Some(aside) = aside {
        emit(Event::StateReset { manifest: true, aside });
    }
    Ok(m)
}

/// The delete queue for a trip; a corrupt one is reset like the manifest. That is safe: deletes
/// are only held, and the next fill derives them again.
fn load_queue(path: &Path, emit: &mut dyn FnMut(Event)) -> io::Result<DeleteQueue> {
    let (q, aside) = load_or_reset(path, unix_now())?;
    if let Some(aside) = aside {
        emit(Event::StateReset { manifest: false, aside });
    }
    Ok(q)
}

/// Delete `rel` under the destination `root` and note it for the snapshot. On an error it is
/// reported and handed back, for the caller to keep queued.
fn delete_at(root: &Path, pair: &str, rel: String, snaps: &mut SnapBatch, emit: &mut dyn FnMut(Event)) -> Option<String> {
    let path = root.join(&rel);
    match remove(&path) {
        Ok(()) => {
            prune_dirs(root, &path);
            snaps.add(pair, Change::Delete { rel: rel.clone() }, emit);
            emit(Event::Deleted { pair: pair.to_string(), rel });
            None
        }
        Err(e) => {
            emit(Event::Error { pair: pair.to_string(), msg: format!("delete {rel}: {e}") });
            Some(rel)
        }
    }
}

pub fn snap_path(snapshots: &Path, pair: &str) -> PathBuf {
    snapshots.join(format!("{pair}.snap"))
}

/// The start of the error for a destination snapshot that couldn't be recorded.
pub fn record_failed(destination: &str) -> String {
    format!("couldn't record what's at {destination}: ")
}

/// The start of a fill's error for a pair whose destination it can't list over the remote address.
pub fn unreachable(destination: &str) -> String {
    format!("can't reach {destination}: ")
}

/// A pair's destination snapshot as a sneakernet fill finds it (spec 4.4).
enum DestinationSnap {
    Found(SnapshotReader),
    /// Never recorded: a first trip.
    First,
    /// Damaged, unreadable, or gone since it was recorded: never an empty destination.
    Lost,
}

/// Open the pair's destination snapshot; a damaged one is set aside, and every problem is logged.
fn open_snapshot(snapshots: &Path, pair: &str, destination: &str, emit: &mut dyn FnMut(Event)) -> DestinationSnap {
    let path = snap_path(snapshots, pair);
    let e = match Snapshot::open(&path) {
        Ok(r) => return DestinationSnap::Found(r),
        Err(e) => e,
    };
    let msg = match e.kind() {
        io::ErrorKind::NotFound if !Snapshot::recorded_before(&path) => return DestinationSnap::First,
        io::ErrorKind::NotFound => format!("the snapshot of {pair} at {destination} is missing"),
        io::ErrorKind::InvalidData => {
            let mut aside = path.as_os_str().to_owned();
            aside.push(format!(".corrupt-{}", unix_now()));
            let aside = PathBuf::from(aside);
            match fs::rename(&path, &aside) {
                Ok(()) => format!("the snapshot of {pair} at {destination} was damaged; set aside as {}", aside.display()),
                Err(e2) => format!("the snapshot of {pair} at {destination} was damaged ({e}) and can't be set aside: {e2}"),
            }
        }
        _ => format!("the snapshot of {pair} at {destination} can't be read: {e}"),
    };
    emit(Event::Info { pair: pair.to_string(), msg });
    DestinationSnap::Lost
}

const SNAP_BATCH: usize = 256;
const SNAP_EVERY: Duration = Duration::from_secs(30);

/// Changes verified at the destination, written to the pairs' snapshots in batches, since each write
/// rewrites a whole snapshot: every `SNAP_BATCH` changes or `SNAP_EVERY`, and on `flush`.
pub(crate) struct SnapBatch<'a> {
    snapshots: &'a Path,
    pending: BTreeMap<String, Vec<Change>>,
    count: usize,
    since: Instant,
}

impl<'a> SnapBatch<'a> {
    pub(crate) fn new(snapshots: &'a Path) -> Self {
        Self { snapshots, pending: BTreeMap::new(), count: 0, since: Instant::now() }
    }

    /// The number of snapshots this rewrote (0 unless the batch was due).
    pub(crate) fn add(&mut self, pair: &str, change: Change, emit: &mut dyn FnMut(Event)) -> usize {
        self.pending.entry(pair.to_string()).or_default().push(change);
        self.count += 1;
        if self.count >= SNAP_BATCH || self.since.elapsed() >= SNAP_EVERY {
            self.flush(emit)
        } else {
            0
        }
    }

    /// Write what is pending; the number of snapshots rewritten. A failed write is logged:
    /// the next visit to the destination records it afresh.
    pub(crate) fn flush(&mut self, emit: &mut dyn FnMut(Event)) -> usize {
        let pending = std::mem::take(&mut self.pending);
        (self.count, self.since) = (0, Instant::now());
        for (pair, changes) in &pending {
            if let Err(e) = Snapshot::apply(&snap_path(self.snapshots, pair), changes) {
                emit(Event::Error { pair: pair.clone(), msg: format!("updating the snapshot: {e}") });
            }
        }
        pending.len()
    }
}

/// Drop the cargo (laptop files, and a part for it) and queued deletes of pair ids
/// that are no longer in the config, and their destination snapshots; and snapshot leftovers (temp and
/// set-aside files) older than 30 days. A file that can't be deleted keeps its entry, to retry.
pub fn purge_unknown(
    known: &[String],
    cargo_dir: &Path,
    manifest_path: &Path,
    deletes_path: &Path,
    snapshots: &Path,
    emit: &mut dyn FnMut(Event),
) -> io::Result<()> {
    let gone = |pair: &str| !known.iter().any(|k| k == pair);
    let mut m = load_manifest(manifest_path, emit)?;
    let mut dropped: BTreeMap<String, u32> = BTreeMap::new();
    let before = m.clone();
    if let Some(p) = m.partial.take_if(|p| gone(&p.pair)) {
        let part = part_of(&cargo_dir.join(&p.pair).join(&p.rel));
        if remove(&part).is_ok() {
            prune_dirs(cargo_dir, &part);
        } else {
            m.partial = Some(p);
        }
    }
    m.items.retain(|i| {
        if !gone(&i.pair) {
            return true;
        }
        let f = cargo_dir.join(&i.pair).join(&i.rel);
        if let Err(e) = remove(&f) {
            emit(Event::Error { pair: i.pair.clone(), msg: format!("{}: {e}", i.rel) });
            return true;
        }
        prune_dirs(cargo_dir, &f);
        *dropped.entry(i.pair.clone()).or_default() += 1;
        false
    });
    if m != before {
        m.save(manifest_path)?;
    }
    for (pair, n) in dropped {
        emit(Event::Info { msg: format!("discarded {n} carried files of {pair}, which is no longer in the folder list"), pair });
    }
    let mut q = load_queue(deletes_path, emit)?;
    let n = q.pairs.len();
    q.pairs.retain(|id, _| !gone(id));
    if q.pairs.len() != n {
        q.save(deletes_path)?;
    }
    const STALE: Duration = Duration::from_secs(30 * 86_400);
    for f in fs::read_dir(snapshots).into_iter().flatten().flatten() {
        let (p, name) = (f.path(), f.file_name().to_string_lossy().into_owned());
        let id = name.strip_suffix(".snap").or_else(|| name.strip_suffix(".snap.recorded"));
        let leftover = name.ends_with(".snap.tmp") || name.contains(".snap.corrupt-");
        let old = || f.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|a| a > STALE));
        if id.is_some_and(gone) || (leftover && old()) {
            if let Err(e) = fs::remove_file(&p) {
                emit(Event::Error { pair: String::new(), msg: format!("{}: {e}", p.display()) });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferry_core::cargo::{CargoItem, Partial};
    use ferry_core::deletes::PairDeletes;

    #[test]
    fn purge_drops_cargo_and_deletes_of_removed_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let (cargo, mp, dp) = (dir.path().join("cargo"), dir.path().join("manifest.json"), dir.path().join("deletes.json"));
        let item = |pair: &str, rel: &str| {
            let f = cargo.join(pair).join(rel);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(&f, b"x").unwrap();
            CargoItem { pair: pair.into(), rel: rel.into(), size: 1, mtime: 1, hash: 1 }
        };
        let items = vec![item("keep", "a.zip"), item("old", r"d\b.zip"), item("old", "c.zip")];
        fs::write(cargo.join("old").join("big.bin.ferry-part"), b"p").unwrap();
        let partial = Some(Partial { pair: "old".into(), rel: "big.bin".into(), size: 9, mtime: 1 });
        CargoManifest { items, partial }.save(&mp).unwrap();
        let mut q = DeleteQueue::default();
        q.pairs.insert("keep".into(), PairDeletes { paths: vec!["x".into()], ..Default::default() });
        q.pairs.insert("old".into(), PairDeletes { paths: vec!["y".into()], ..Default::default() });
        q.save(&dp).unwrap();

        let mut events = vec![];
        purge_unknown(&["keep".into()], &cargo, &mp, &dp, &dir.path().join("snapshots"), &mut |e| events.push(e)).unwrap();

        let m = CargoManifest::load(&mp).unwrap();
        assert_eq!(m.items.iter().map(|i| i.pair.as_str()).collect::<Vec<_>>(), ["keep"]);
        assert_eq!(m.partial, None);
        assert!(cargo.join("keep").join("a.zip").exists());
        assert!(!cargo.join("old").exists(), "laptop files, the part and their folders are gone");
        assert_eq!(DeleteQueue::load(&dp).unwrap().pairs.keys().collect::<Vec<_>>(), ["keep"]);
        assert!(events.iter().any(|e| matches!(e, Event::Info { pair, .. } if pair == "old")));
    }

    #[test]
    fn purge_drops_snapshots_of_removed_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let snaps = dir.path().join("snapshots");
        fs::create_dir_all(&snaps).unwrap();
        for f in ["keep.snap", "old.snap", "keep.snap.corrupt-1"] {
            fs::write(snaps.join(f), b"x").unwrap();
        }
        let (mp, dp) = (dir.path().join("manifest.json"), dir.path().join("deletes.json"));
        purge_unknown(&["keep".into()], &dir.path().join("cargo"), &mp, &dp, &snaps, &mut |_| {}).unwrap();
        let mut left: Vec<String> = fs::read_dir(&snaps).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["keep.snap", "keep.snap.corrupt-1"]);
        // No snapshot folder yet is fine.
        purge_unknown(&[], &dir.path().join("cargo"), &mp, &dp, &dir.path().join("none"), &mut |_| {}).unwrap();
    }

    #[test]
    fn purge_drops_old_snapshot_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let snaps = dir.path().join("snapshots");
        fs::create_dir_all(&snaps).unwrap();
        let old = SystemTime::now() - Duration::from_secs(31 * 86_400);
        for (f, t) in [
            ("keep.snap.tmp", Some(old)),
            ("keep.snap.corrupt-1", Some(old)),
            ("keep.snap.corrupt-2", None),
            ("keep.snap.recorded", Some(old)),
            ("gone.snap.recorded", None),
            ("keep.snap", Some(old)),
        ] {
            fs::write(snaps.join(f), b"x").unwrap();
            if let Some(t) = t {
                fs::OpenOptions::new().write(true).open(snaps.join(f)).unwrap().set_modified(t).unwrap();
            }
        }
        let (mp, dp) = (dir.path().join("manifest.json"), dir.path().join("deletes.json"));
        purge_unknown(&["keep".into()], &dir.path().join("cargo"), &mp, &dp, &snaps, &mut |_| {}).unwrap();
        let mut left: Vec<String> = fs::read_dir(&snaps).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, ["keep.snap", "keep.snap.corrupt-2", "keep.snap.recorded"]);
    }

    #[test]
    fn snapshot_changes_are_batched() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("home");
        fs::create_dir_all(&destination).unwrap();
        let snaps = dir.path().join("snapshots");
        Snapshot::record(&destination, &snap_path(&snaps, "p")).unwrap();
        let mut batch = SnapBatch::new(&snaps);
        let mut events = vec![];
        let mut rewrites = 0;
        for i in 0..300 {
            let rel = format!("new{}{i:03}.zip", std::path::MAIN_SEPARATOR);
            rewrites += batch.add("p", Change::Push { rel, size: 1, mtime: 1 }, &mut |e| events.push(e));
        }
        rewrites += batch.flush(&mut |e| events.push(e));
        assert!(rewrites <= 2, "{rewrites} rewrites");
        assert!(events.is_empty(), "{events:?}");
        let mut r = Snapshot::open(&snap_path(&snaps, "p")).unwrap();
        assert_eq!(r.dir("new").unwrap().len(), 300);
        assert_eq!(batch.flush(&mut |_| {}), 0, "nothing pending");
    }

    #[test]
    fn corrupt_state_is_reset_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (mp, dp) = (dir.path().join("manifest.json"), dir.path().join("deletes.json"));
        fs::write(&mp, b"not json").unwrap();
        fs::write(&dp, b"{").unwrap();
        let mut events = vec![];
        purge_unknown(&[], &dir.path().join("cargo"), &mp, &dp, &dir.path().join("snapshots"), &mut |e| events.push(e)).unwrap();
        let resets: Vec<bool> = events.iter().filter_map(|e| match e { Event::StateReset { manifest, .. } => Some(*manifest), _ => None }).collect();
        assert_eq!(resets, [true, false]);
        assert!(!mp.exists() && !dp.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".corrupt-")).count(), 2);
    }

    #[test]
    fn prune_stops_at_root_and_at_non_empty_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("a").join("b")).unwrap();
        fs::write(root.join("a").join("keep.txt"), b"x").unwrap();
        prune_dirs(&root, &root.join("a").join("b").join("gone.zip"));
        assert!(!root.join("a").join("b").exists());
        assert!(root.join("a").exists());
        fs::remove_file(root.join("a").join("keep.txt")).unwrap();
        prune_dirs(&root, &root.join("a").join("keep.txt"));
        assert!(!root.join("a").exists());
        assert!(root.exists());
    }
}
