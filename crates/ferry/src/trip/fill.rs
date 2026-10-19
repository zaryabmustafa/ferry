use super::{
    delete_at, load_manifest, load_queue, open_snapshot, part_of, unreachable, remove, Event, FillReport, DestinationSnap, SnapBatch, TripCtx,
};
use crate::win::copy::{copy_hashed, finalize};
use ferry_core::cargo::{CargoItem, CargoManifest, Partial};
use ferry_core::deletes::{decide, DeleteDecision, PairDeletes};
use ferry_core::diff::{diff_pair, Candidate, DestinationView};
use ferry_core::entry::{fold, long_path};
use ferry_core::load::{budget, pick_load};
use ferry_core::snapshot::SnapshotReader;
use std::fs;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::Ordering;

/// Spec 4.4: a fill with no remote address for the destination and no usable destination snapshot.
pub fn visit_destination(destination: &str) -> String {
    format!("Visit {destination} once so Ferry can record what's there.")
}

/// `rel` is definitely not at the source: not found, or under a name that is now a file.
fn gone_at_source(source: &Path, rel: &str) -> bool {
    fs::symlink_metadata(long_path(&source.join(rel))).is_err_and(|e| matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory))
}

fn same_version(p: &Partial, c: &Candidate) -> bool {
    p.pair == c.pair && fold(&p.rel) == fold(&c.rel) && p.size == c.size && p.mtime == c.mtime
}

/// Source walk at the source (LAN), the destination listed over its remote address, or read from
/// its snapshot without one (spec 4.4); deletes queued per pair and, with a remote address, applied
/// on `destination_meta` once the pair is caught up; then fill cargo, pairs in priority order.
pub fn run_fill(ctx: &TripCtx, emit: &mut dyn FnMut(Event)) -> io::Result<FillReport> {
    let opts = ctx.opts;
    let mut manifest = load_manifest(ctx.manifest_path, emit)?;
    let mut queue = load_queue(ctx.deletes_path, emit)?;
    let mut report = FillReport::default();
    // For each pair that diffed cleanly, in priority order: its candidates, and (id, unstable).
    let mut carries: Vec<Vec<Candidate>> = vec![];
    let mut diffed: Vec<(&str, u64)> = vec![];
    let mut asked_destination = false;
    let mut snaps = SnapBatch::new(ctx.snapshots);

    for roots in ctx.pairs {
        if ctx.cancel.load(Ordering::Relaxed) {
            break;
        }
        let pair = roots.id.as_str();
        let err = |msg: String| Event::Error { pair: pair.to_string(), msg };
        let mut snap = SnapshotReader::default();
        // The live view of the destination, when it has a remote address.
        let destination_meta = roots.destination_meta.as_deref().filter(|_| ctx.destination_remote_configured);
        let destination = match destination_meta {
            // Windows reports an unreachable host or share as NotFound, which the diff would read
            // as an empty destination and re-carry everything. A root we can create (or that exists) is real.
            Some(destination_meta) => match fs::create_dir_all(long_path(destination_meta)) {
                Ok(()) => DestinationView::Live(destination_meta),
                Err(e) => {
                    emit(err(format!("{}{e}", unreachable(ctx.destination_name))));
                    continue;
                }
            },
            None if ctx.destination_remote_configured => {
                emit(err(format!("{}no remote root", unreachable(ctx.destination_name))));
                continue;
            }
            None => {
                match open_snapshot(ctx.snapshots, pair, ctx.destination_name, emit) {
                    DestinationSnap::Found(r) => snap = r,
                    // Only a first trip may start from an empty destination, if the user said so.
                    DestinationSnap::First if opts.destination_starts_empty => {}
                    DestinationSnap::First | DestinationSnap::Lost => {
                        if !asked_destination {
                            asked_destination = true;
                            emit(Event::Error { pair: String::new(), msg: visit_destination(ctx.destination_name) });
                        }
                        continue;
                    }
                }
                DestinationView::Snapshot(&mut snap)
            }
        };
        let stable_secs = i64::from(opts.stable_minutes) * 60;
        let mut d = match diff_pair(pair, &roots.source, destination, &manifest, ctx.now, stable_secs) {
            Ok(d) => d,
            Err(e) => {
                emit(err(format!("listing failed: {e}")));
                continue;
            }
        };

        for rel in &d.case_clashes {
            let (src, dest) = (ctx.source_name, ctx.destination_name);
            emit(err(format!("{rel}: another file at {src} has the same name but for case; Windows can keep only one, so neither is copied and deletes at {dest} wait")));
        }
        // A listing can leave out files that are there (Access-Based Enumeration, Samba's `hide
        // unreadable`): only a file the source can't find when asked by name is deleted.
        let listed = d.deletes.len();
        d.deletes.retain(|rel| gone_at_source(&roots.source, rel));
        if d.deletes.len() < listed {
            let n = listed - d.deletes.len();
            let msg = format!("{n} files missing from the listing of {pair} at {} are still there when asked by name, so they stay at {}", ctx.source_name, ctx.destination_name);
            emit(Event::Info { pair: pair.to_string(), msg });
        }

        for rel in &d.cargo_gone {
            match remove(&ctx.cargo_dir.join(pair).join(rel)) {
                Ok(()) => manifest.remove(pair, rel),
                Err(e) => emit(err(format!("{rel}: {e}"))),
            }
        }
        manifest.save(ctx.manifest_path)?;

        let q = queue.pairs.entry(pair.to_string()).or_default();
        let trusted = d.complete && (d.source_files > 0 || roots.allow_empty);
        if d.complete && d.source_files == 0 && !roots.allow_empty && !d.deletes.is_empty() {
            emit(Event::Info { pair: pair.to_string(), msg: format!("folder {pair} looks empty at {}; deletes held", ctx.source_name) });
        }
        if !trusted {
            // Unload can't re-check the listing, so an untrusted one leaves nothing queued.
            *q = PairDeletes::default();
        } else if q.paths != d.deletes {
            // A confirmation covers one exact set of deletes.
            q.paths = d.deletes;
            q.confirmed = false;
        }
        if trusted {
            q.destination_files = d.destination_files;
        }
        q.outstanding = d.carry.len() as u64 + d.unstable;
        let in_cargo = manifest.items.iter().filter(|i| i.pair == pair).count();
        let decision = decide(
            d.complete,
            d.source_files,
            roots.allow_empty,
            d.destination_files,
            q.paths.len(),
            q.outstanding,
            in_cargo,
            q.confirmed,
            opts.delete_confirm_percent,
            opts.delete_confirm_count,
        );
        q.needs_confirm = decision == DeleteDecision::NeedsConfirm;
        match (decision, destination_meta) {
            (DeleteDecision::Apply, Some(destination_meta)) => {
                let mut failed = vec![];
                for rel in std::mem::take(&mut q.paths) {
                    match delete_at(destination_meta, pair, rel, &mut snaps, emit) {
                        None => report.deleted += 1,
                        Some(rel) => failed.push(rel),
                    }
                }
                snaps.flush(emit);
                q.paths = failed;
                q.confirmed = false;
            }
            (DeleteDecision::NeedsConfirm, _) => emit(Event::HeldForConfirm { pair: pair.to_string(), count: q.paths.len() }),
            // Without a view of the destination (spec 4.4) they stay queued for the unload.
            (DeleteDecision::Apply, None) | (DeleteDecision::Hold, _) => {}
        }
        queue.save(ctx.deletes_path)?;
        carries.push(d.carry);
        diffed.push((pair, d.unstable));
    }

    let room = budget(opts.cap_bytes, manifest.carried_bytes(), (ctx.disk_free)(), opts.min_free_bytes);
    let mut load = pick_load(&carries, room, opts.cap_bytes);
    // Spec 6.6: a part of this exact version is resumed first, before a newer file can replace it.
    if let Some(i) = manifest.partial.as_ref().and_then(|p| load.picked.iter().position(|c| same_version(p, c))) {
        let c = load.picked.remove(i);
        load.picked.insert(0, c);
    }
    for c in &load.oversize {
        emit(Event::Oversize { pair: c.pair.clone(), rel: c.rel.clone(), size: c.size });
    }
    for c in &load.picked {
        if !ctx.go_on() {
            break;
        }
        let Some(roots) = ctx.pairs.iter().find(|r| r.id == c.pair) else { continue };
        match carry(ctx, &roots.source.join(&c.rel), c, &mut manifest, emit) {
            Ok(true) => {
                report.carried += 1;
                report.carried_bytes += c.size;
                emit(Event::Carried { pair: c.pair.clone(), rel: c.rel.clone(), size: c.size });
            }
            Ok(false) => {}
            Err(_) if ctx.cancel.load(Ordering::Relaxed) => break,
            Err(e) => emit(Event::Error { pair: c.pair.clone(), msg: format!("{}: {e}", c.rel) }),
        }
    }

    // What is still not on the laptop (or not stable yet) holds that pair's deletes.
    for (carry, (pair, unstable)) in carries.iter().zip(&diffed) {
        let left: Vec<&Candidate> = carry.iter().filter(|c| !manifest.has(&c.pair, &c.rel, c.size, c.mtime)).collect();
        report.outstanding_bytes += left.iter().map(|c| c.size).sum::<u64>();
        if let Some(q) = queue.pairs.get_mut(*pair) {
            q.outstanding = left.len() as u64 + unstable;
            // Say what holds them: a file too big for the cap, or locked, would otherwise hold
            // them forever without a word.
            if !q.paths.is_empty() && q.outstanding > 0 {
                let first = left.first().map_or_else(|| "files still being written".to_string(), |c| c.rel.clone());
                let (n, dest) = (q.paths.len(), ctx.destination_name);
                let msg = format!("{n} deletes at {dest} wait until {} more files are on the laptop, starting with {first}", q.outstanding);
                emit(Event::Info { pair: pair.to_string(), msg });
            }
        }
    }
    queue.save(ctx.deletes_path)?;
    Ok(report)
}

/// Copy one candidate into cargo. `Ok(false)`: it changed at the source during the copy and
/// was dropped. An error under cancel keeps the part and `partial` for resume; any other
/// outcome removes the part (best effort) and clears `partial`.
fn carry(
    ctx: &TripCtx,
    src: &Path,
    c: &Candidate,
    manifest: &mut CargoManifest,
    emit: &mut dyn FnMut(Event),
) -> io::Result<bool> {
    let dst = ctx.cargo_dir.join(&c.pair).join(&c.rel);
    let part = part_of(&dst);
    if !manifest.partial.as_ref().is_some_and(|p| same_version(p, c)) {
        // Only a part of this exact version may be resumed.
        if let Some(old) = manifest.partial.take() {
            remove(&part_of(&ctx.cargo_dir.join(&old.pair).join(&old.rel)))?;
        }
        remove(&part)?;
        manifest.partial = Some(Partial { pair: c.pair.clone(), rel: c.rel.clone(), size: c.size, mtime: c.mtime });
        manifest.save(ctx.manifest_path)?;
    }

    let r = copy_one(ctx, src, &part, &dst, c, emit);
    if r.is_err() && ctx.cancel.load(Ordering::Relaxed) {
        return r.map(|_| false);
    }
    // Best effort: must not mask the copy's own result. A leftover part is deleted before
    // its file is next copied, since it no longer matches `partial`.
    let _ = remove(&part);
    manifest.partial = None;
    if let Ok(Some(hash)) = r {
        // Replaces any older version of this file already carried.
        manifest.remove(&c.pair, &c.rel);
        manifest.items.push(CargoItem { pair: c.pair.clone(), rel: c.rel.clone(), size: c.size, mtime: c.mtime, hash });
    }
    manifest.save(ctx.manifest_path)?;
    r.map(|h| h.is_some())
}

/// `Ok(None)` if the source file's size or mtime moved during the copy (or it is gone),
/// whether or not the copy itself failed.
fn copy_one(
    ctx: &TripCtx,
    src: &Path,
    part: &Path,
    dst: &Path,
    c: &Candidate,
    emit: &mut dyn FnMut(Event),
) -> io::Result<Option<u128>> {
    if let Some(dir) = dst.parent() {
        fs::create_dir_all(long_path(dir))?;
    }
    let copied = copy_hashed(src, part, ctx.cancel, &mut |done| {
        emit(Event::Progress { pair: c.pair.clone(), rel: c.rel.clone(), done, total: c.size });
        ctx.go_on(); // spec 4.3: a path that stops being allowed cancels the copy
    });
    let unchanged = fs::metadata(long_path(src))
        .is_ok_and(|m| m.len() == c.size && m.last_write_time() as i64 == c.mtime);
    if !unchanged {
        return Ok(None);
    }
    let copied = copied?;
    finalize(part, dst, c.mtime)?;
    Ok(Some(copied))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trip::PairRoots;
    use ferry_core::cargo::Partial;
    use ferry_core::config::{Options, GB};
    use ferry_core::deletes::{DeleteQueue, PairDeletes};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    const KB: usize = 1024;
    const MIB: usize = 1 << 20;

    /// Deny listing a directory by holding it open without sharing until dropped. (A deny ACL
    /// doesn't stop an elevated admin, such as the CI runner.)
    struct Locked {
        _dir: std::fs::File,
    }
    impl Locked {
        fn new(p: PathBuf) -> Locked {
            use std::os::windows::fs::OpenOptionsExt;
            // No sharing; FILE_FLAG_BACKUP_SEMANTICS lets a directory be opened.
            Locked { _dir: std::fs::OpenOptions::new().read(true).share_mode(0).custom_flags(0x0200_0000).open(p).unwrap() }
        }
    }

    struct Env {
        source: TempDir,
        destination: TempDir,
        laptop: TempDir,
        /// mtime for every test file: a day old, so stable.
        t: SystemTime,
        opts: Options,
        disk_free: u64,
        allow_empty: bool,
        /// Trip time in FILETIME ticks; the default puts every file past the stability window.
        now: i64,
        /// Overrides the remote destination root (default: `destination`).
        destination_meta: Option<PathBuf>,
        /// No remote address for the destination: the fill diffs against the destination snapshot.
        sneakernet: bool,
        /// Pair ids; every pair uses the same source and destination folders.
        ids: &'static [&'static str],
    }

    impl Env {
        fn new() -> Env {
            Env {
                source: tempfile::tempdir().unwrap(),
                destination: tempfile::tempdir().unwrap(),
                laptop: tempfile::tempdir().unwrap(),
                t: SystemTime::now() - Duration::from_secs(86_400),
                opts: Options { cap_bytes: GB, min_free_bytes: 0, ..Options::default() },
                disk_free: u64::MAX,
                allow_empty: false,
                now: i64::MAX,
                destination_meta: None,
                sneakernet: false,
                ids: &["p"],
            }
        }
        fn put(&self, root: &Path, name: &str, len: usize, fill: u8) -> PathBuf {
            let p = root.join(name);
            fs::write(&p, vec![fill; len]).unwrap();
            fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(self.t).unwrap();
            p
        }
        fn source(&self, name: &str, len: usize) -> PathBuf {
            self.put(self.source.path(), name, len, b'o')
        }
        /// The same file at the source and the destination: same size and mtime.
        fn synced(&self, name: &str, len: usize) {
            self.source(name, len);
            self.put(self.destination.path(), name, len, b'o');
        }
        fn cargo(&self) -> PathBuf {
            self.laptop.path().join("cargo")
        }
        fn manifest_path(&self) -> PathBuf {
            self.laptop.path().join("manifest.json")
        }
        fn deletes_path(&self) -> PathBuf {
            self.laptop.path().join("deletes.json")
        }
        fn snapshots(&self) -> PathBuf {
            self.laptop.path().join("snapshots")
        }
        /// Record the destination as it is now, as a visit to the destination would.
        fn record_destination(&self) {
            ferry_core::snapshot::Snapshot::record(self.destination.path(), &self.snapshots().join("p.snap")).unwrap();
        }
        fn manifest(&self) -> CargoManifest {
            CargoManifest::load(&self.manifest_path()).unwrap()
        }
        fn queue(&self) -> DeleteQueue {
            DeleteQueue::load(&self.deletes_path()).unwrap()
        }
        /// Seed the queue as if an earlier trusted trip had confirmed these deletes.
        fn seed_queue(&self, paths: &[&str]) {
            let mut q = DeleteQueue::default();
            q.pairs.insert("p".into(), PairDeletes {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                confirmed: true,
                needs_confirm: true,
                destination_files: 1,
                ..Default::default()
            });
            q.save(&self.deletes_path()).unwrap();
        }
        fn fill_with(&self, cancel: &AtomicBool, emit: &mut dyn FnMut(Event)) -> FillReport {
            self.fill_path(cancel, &|| true, emit)
        }
        fn fill_path(&self, cancel: &AtomicBool, path_ok: &dyn Fn() -> bool, emit: &mut dyn FnMut(Event)) -> FillReport {
            let pairs: Vec<PairRoots> = self
                .ids
                .iter()
                .map(|id| PairRoots {
                    id: (*id).into(),
                    source: self.source.path().into(),
                    source_meta: Some(self.source.path().into()),
                    destination: self.destination.path().into(),
                    destination_meta: (!self.sneakernet).then(|| self.destination_meta.clone().unwrap_or_else(|| self.destination.path().into())),
                    allow_empty: self.allow_empty,
                })
                .collect();
            let (cargo, mp, dp, snaps) = (self.cargo(), self.manifest_path(), self.deletes_path(), self.snapshots());
            let free = self.disk_free;
            let ctx = TripCtx {
                pairs: &pairs,
                cargo_dir: &cargo,
                manifest_path: &mp,
                deletes_path: &dp,
                snapshots: &snaps,
                source_remote_configured: true,
                destination_remote_configured: !self.sneakernet,
                source_name: "HQ",
                destination_name: "Lake house",
                opts: &self.opts,
                disk_free: &move || free,
                now: self.now,
                cancel,
                path_ok,
            };
            run_fill(&ctx, emit).unwrap()
        }
        fn fill(&self) -> (FillReport, Vec<Event>) {
            let mut events = vec![];
            let r = self.fill_with(&AtomicBool::new(false), &mut |e| events.push(e));
            (r, events)
        }
    }

    #[test]
    fn deletes_held_by_a_file_that_cant_be_carried_say_so() {
        let mut env = Env::new();
        env.opts.cap_bytes = 4 * KB as u64;
        env.source("huge.bin", 8 * KB);
        env.put(env.destination.path(), "old.txt", KB, b'h');
        let mut ev = vec![];
        env.fill_with(&AtomicBool::new(false), &mut |e| ev.push(e));
        let want = "1 deletes at Lake house wait until 1 more files are on the laptop, starting with huge.bin";
        assert!(ev.iter().any(|e| matches!(e, Event::Info { msg, .. } if msg == want)), "{ev:?}");
    }

    #[test]
    fn only_a_file_the_source_cannot_find_counts_as_gone() {
        let env = Env::new();
        env.source("here.txt", KB);
        let src = env.source.path();
        assert!(!gone_at_source(src, "here.txt"));
        assert!(gone_at_source(src, "gone.txt"));
        assert!(gone_at_source(src, r"no\such\dir.txt"));
        // A folder at the destination where the source now has a file of that name.
        assert!(gone_at_source(src, r"here.txt\old.txt"));
    }

    #[test]
    fn carries_missing_files_within_cap() {
        let mut env = Env::new();
        env.opts.cap_bytes = 10 * KB as u64;
        for n in ["a.zip", "b.zip", "c.zip"] {
            env.source(n, 4 * KB);
        }
        let (r, _) = env.fill();
        assert_eq!((r.carried, r.carried_bytes), (2, 8 * KB as u64));
        let m = env.manifest();
        assert_eq!(m.items.len(), 2);
        assert_eq!(m.partial, None);
        for i in &m.items {
            assert_eq!(fs::read(env.cargo().join("p").join(&i.rel)).unwrap(), vec![b'o'; 4 * KB]);
        }
        assert_eq!(r.outstanding_bytes, 4 * KB as u64);
    }

    #[test]
    fn second_fill_carries_nothing_new() {
        let env = Env::new();
        env.source("a.zip", 4 * KB);
        env.source("b.zip", 4 * KB);
        assert_eq!(env.fill().0.carried, 2);
        assert_eq!(env.fill().0.carried, 0);
        assert_eq!(env.manifest().items.len(), 2);
    }

    #[test]
    fn applies_deletes_when_pair_caught_up() {
        let mut env = Env::new();
        // 1 of 2 destination files is over the default 10% confirm threshold; this test is about timing.
        env.opts.delete_confirm_percent = 100;
        env.synced("a.zip", 4 * KB);
        env.put(env.destination.path(), "stale.zip", 4 * KB, b's');
        let (r, ev) = env.fill();
        assert!(!env.destination.path().join("stale.zip").exists());
        assert!(env.destination.path().join("a.zip").exists());
        assert_eq!(r.deleted, 1);
        assert!(ev.contains(&Event::Deleted { pair: "p".into(), rel: "stale.zip".into() }));
        assert!(env.queue().pairs["p"].paths.is_empty());
    }

    #[test]
    fn holds_deletes_while_files_outstanding() {
        let env = Env::new();
        env.source("new.zip", 4 * KB);
        env.put(env.destination.path(), "old.zip", 4 * KB, b's');
        let (r, _) = env.fill();
        assert_eq!(r.carried, 1);
        assert!(env.destination.path().join("old.zip").exists());
        let q = &env.queue().pairs["p"];
        assert_eq!(q.paths, ["old.zip"]);
        assert_eq!(q.outstanding, 0, "new.zip is in cargo now; deletes wait for unload");
    }

    #[test]
    fn holds_deletes_when_source_root_empty() {
        let env = Env::new();
        env.put(env.destination.path(), "x.zip", 4 * KB, b's');
        env.seed_queue(&["x.zip"]);
        let (r, _) = env.fill();
        assert_eq!(r.deleted, 0);
        assert!(env.destination.path().join("x.zip").exists());
        // An untrusted listing clears the queue rather than (re)writing it.
        let q = &env.queue().pairs["p"];
        assert!(q.paths.is_empty());
        assert!(!q.confirmed && !q.needs_confirm);
    }

    #[test]
    fn incomplete_listing_clears_queued_deletes() {
        let mut env = Env::new();
        env.opts.delete_confirm_percent = 100;
        env.synced("ok.zip", KB);
        fs::create_dir(env.source.path().join("locked")).unwrap();
        env.put(&env.source.path().join("locked"), "a.zip", KB, b'o');
        env.put(env.destination.path(), "stale.zip", KB, b's');
        env.seed_queue(&["stale.zip"]);
        let _lock = Locked::new(env.source.path().join("locked"));
        assert!(fs::read_dir(env.source.path().join("locked")).is_err(), "lock did not take effect");
        let (r, _) = env.fill();
        assert_eq!(r.deleted, 0);
        assert!(env.destination.path().join("stale.zip").exists());
        let q = &env.queue().pairs["p"];
        assert!(q.paths.is_empty());
        assert!(!q.confirmed && !q.needs_confirm);
    }

    #[test]
    fn holds_deletes_while_source_file_unstable() {
        let mut env = Env::new();
        // Only the unstable new.zip may hold old.zip, not the confirm threshold.
        env.opts.delete_confirm_percent = 100;
        let new = env.source("new.zip", 4 * KB);
        let mtime = fs::metadata(&new).unwrap().last_write_time() as i64;
        env.now = mtime + 1; // just written: inside the stability window
        env.put(env.destination.path(), "old.zip", 4 * KB, b's');
        let (r, _) = env.fill();
        assert_eq!((r.carried, r.deleted), (0, 0));
        assert!(env.destination.path().join("old.zip").exists());
        let q = &env.queue().pairs["p"];
        assert_eq!(q.paths, ["old.zip"]);
        assert_eq!(q.outstanding, 1);
        assert!(!q.needs_confirm);
    }

    #[test]
    fn mass_delete_needs_confirm() {
        let env = Env::new();
        env.synced("keep.zip", KB);
        for i in 0..60 {
            env.put(env.destination.path(), &format!("gone-{i:02}.zip"), KB, b's');
        }
        let (r, ev) = env.fill();
        assert!(ev.contains(&Event::HeldForConfirm { pair: "p".into(), count: 60 }));
        assert_eq!(r.deleted, 0);
        assert_eq!(fs::read_dir(env.destination.path()).unwrap().count(), 61);
        let q = &env.queue().pairs["p"];
        assert!(q.needs_confirm);
        assert_eq!(q.destination_files, 61);
    }

    #[test]
    fn file_changed_during_copy_is_discarded() {
        // Windows and SMB writers are shut out while a file is read (`copy::open_unbuffered_read`),
        // but a program on the NAS itself is not, and neither is a change of attributes. A new
        // mtime is caught by the re-stat after the copy: an expected discard, not an error.
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        let env = Env::new();
        let src = env.source("big.bin", 3 * MIB);
        let mut changed = false;
        let mut events = vec![];
        let r = env.fill_with(&AtomicBool::new(false), &mut |e| {
            if matches!(e, Event::Progress { .. }) && !changed {
                changed = true;
                let f = fs::OpenOptions::new().access_mode(FILE_WRITE_ATTRIBUTES).open(&src).unwrap();
                f.set_modified(env.t - Duration::from_secs(3600)).unwrap();
            }
            events.push(e);
        });
        assert!(changed, "no Progress fired");
        assert_eq!(r.carried, 0);
        let m = env.manifest();
        assert!(m.items.is_empty());
        assert_eq!(m.partial, None);
        assert!(!env.cargo().join("p").join("big.bin").exists());
        assert!(!env.cargo().join("p").join("big.bin.ferry-part").exists());
        assert!(!events.iter().any(|e| matches!(e, Event::Error { .. })));
    }

    #[test]
    fn cancel_leaves_part_and_no_manifest_entry() {
        let env = Env::new();
        env.source("big.bin", 3 * MIB);
        let cancel = AtomicBool::new(false);
        let r = env.fill_with(&cancel, &mut |e| {
            if matches!(e, Event::Progress { .. }) {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
        assert_eq!(r.carried, 0);
        assert!(env.cargo().join("p").join("big.bin.ferry-part").exists());
        let m = env.manifest();
        assert!(m.items.is_empty());
        assert_eq!(m.partial.map(|p| p.rel), Some("big.bin".into()));
    }

    #[test]
    fn path_check_failure_cancels_fill_and_keeps_part() {
        let env = Env::new();
        env.source("big.bin", 3 * MIB);
        let cancel = AtomicBool::new(false);
        let lost = std::cell::Cell::new(false);
        let r = env.fill_path(&cancel, &|| !lost.get(), &mut |e| {
            if matches!(e, Event::Progress { .. }) {
                lost.set(true); // the path turns relayed mid-copy
            }
        });
        assert_eq!(r.carried, 0);
        assert!(cancel.load(Ordering::Relaxed), "a lost path cancels the trip");
        assert!(env.cargo().join("p").join("big.bin.ferry-part").exists(), "kept for resume");
        let m = env.manifest();
        assert!(m.items.is_empty());
        assert_eq!(m.partial.map(|p| p.rel), Some("big.bin".into()));
    }

    #[test]
    fn route_change_between_files_stops_the_fill() {
        use crate::trip::job::{same_route, PathGuard};
        use std::time::Instant;
        let env = Env::new();
        env.source("a.zip", 4 * KB);
        env.source("b.zip", 4 * KB);
        let route = std::cell::Cell::new(Some(4));
        let ip = "192.0.2.10".parse().unwrap();
        let guard = PathGuard::new(same_route(|_| route.get(), ip, Some(4)), || Ok(()), Instant::now());
        let cancel = AtomicBool::new(false);
        let r = env.fill_path(&cancel, &|| guard.ok(Instant::now()), &mut |e| {
            if matches!(e, Event::Carried { .. }) {
                route.set(Some(9)); // the route moves into a VPN between the two files
            }
        });
        assert_eq!(r.carried, 1, "the second file is not copied");
        assert!(cancel.load(Ordering::Relaxed));
        assert_eq!(env.manifest().items.len(), 1);
        assert!(guard.into_lost().is_some());
    }

    #[test]
    fn path_checked_before_each_file() {
        let env = Env::new();
        env.source("a.zip", 4 * KB);
        env.source("b.zip", 4 * KB);
        let cancel = AtomicBool::new(false);
        let mut events = vec![];
        let r = env.fill_path(&cancel, &|| false, &mut |e| events.push(e));
        assert_eq!(r.carried, 0);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!events.iter().any(|e| matches!(e, Event::Progress { .. })), "not a byte copied");
        assert!(env.manifest().items.is_empty() && env.manifest().partial.is_none());
        assert!(!env.cargo().join("p").exists());
    }

    #[test]
    fn respects_disk_free_floor() {
        let mut env = Env::new();
        env.opts.min_free_bytes = GB;
        env.disk_free = GB + 4 * KB as u64;
        env.source("a.zip", 4 * KB);
        env.source("b.zip", 4 * KB);
        let (r, _) = env.fill();
        assert_eq!((r.carried, r.carried_bytes), (1, 4 * KB as u64));
    }

    #[test]
    fn stale_part_for_other_version_is_not_resumed() {
        let env = Env::new();
        let src = env.source("a.zip", 8 * KB);
        // A part left by an older version of a.zip: same size, other mtime and bytes.
        fs::create_dir_all(env.cargo().join("p")).unwrap();
        fs::write(env.cargo().join("p").join("a.zip.ferry-part"), vec![b'x'; 8 * KB]).unwrap();
        let stale = Partial { pair: "p".into(), rel: "a.zip".into(), size: 8 * KB as u64, mtime: 1 };
        CargoManifest { partial: Some(stale), ..Default::default() }.save(&env.manifest_path()).unwrap();
        assert_eq!(env.fill().0.carried, 1);
        assert_eq!(fs::read(env.cargo().join("p").join("a.zip")).unwrap(), fs::read(&src).unwrap());
    }

    #[test]
    fn interrupted_file_resumes_before_a_newer_one() {
        let env = Env::new();
        env.source("big.bin", 3 * MIB);
        let cancel = AtomicBool::new(false);
        env.fill_with(&cancel, &mut |e| {
            if matches!(e, Event::Progress { .. }) {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });
        assert!(env.cargo().join("p").join("big.bin.ferry-part").exists());
        // A newer file arrives at the source: newest-first would pick it first and drop the part.
        let new = env.source("new.zip", 4 * KB);
        fs::OpenOptions::new().write(true).open(&new).unwrap().set_modified(env.t + Duration::from_secs(3600)).unwrap();
        let mut first = None;
        let r = env.fill_with(&AtomicBool::new(false), &mut |e| {
            if let Event::Progress { rel, done, .. } = e {
                first.get_or_insert((rel, done));
            }
        });
        assert_eq!(r.carried, 2);
        let (rel, done) = first.unwrap();
        assert_eq!(rel, "big.bin");
        assert!(done > 0, "the part was resumed, not restarted");
    }

    #[test]
    fn empty_source_folder_notes_held_deletes() {
        let env = Env::new();
        env.put(env.destination.path(), "x.zip", 4 * KB, b's');
        let (_, ev) = env.fill();
        let notes: Vec<_> = ev.iter().filter(|e| matches!(e, Event::Info { .. })).collect();
        assert_eq!(notes, [&Event::Info { pair: "p".into(), msg: "folder p looks empty at HQ; deletes held".into() }]);
        assert!(env.destination.path().join("x.zip").exists());
    }

    #[test]
    fn applied_deletes_remove_emptied_destination_folders() {
        let mut env = Env::new();
        env.opts.delete_confirm_percent = 100;
        env.synced("a.zip", 4 * KB);
        fs::create_dir_all(env.destination.path().join("old").join("deep")).unwrap();
        env.put(&env.destination.path().join("old").join("deep"), "stale.zip", KB, b's');
        assert_eq!(env.fill().0.deleted, 1);
        assert!(!env.destination.path().join("old").exists());
        assert!(env.destination.path().exists());
    }

    #[test]
    fn corrupt_state_files_are_reset_and_the_fill_goes_on() {
        let env = Env::new();
        env.source("a.zip", 4 * KB);
        fs::write(env.manifest_path(), b"not json").unwrap();
        fs::write(env.deletes_path(), b"{\"pairs\":").unwrap();
        let (r, ev) = env.fill();
        assert_eq!(r.carried, 1);
        let resets: Vec<bool> = ev.iter().filter_map(|e| match e { Event::StateReset { manifest, .. } => Some(*manifest), _ => None }).collect();
        assert_eq!(resets, [true, false]);
        assert_eq!(env.manifest().items.len(), 1);
        let aside = fs::read_dir(env.laptop.path()).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".corrupt-")).count();
        assert_eq!(aside, 2);
    }

    #[test]
    fn unreachable_destination_skips_pair() {
        let mut env = Env::new();
        env.source("a.zip", 4 * KB);
        env.seed_queue(&["x.zip"]);
        // A destination root that can't exist (its parent is a file): it must not read as empty.
        fs::write(env.destination.path().join("afile"), b"x").unwrap();
        env.destination_meta = Some(env.destination.path().join("afile").join("home"));
        let (r, ev) = env.fill();
        assert_eq!(r.carried, 0);
        assert!(env.manifest().items.is_empty());
        assert!(ev.iter().any(|e| matches!(e, Event::Error { .. })));
        let q = &env.queue().pairs["p"];
        assert_eq!(q.paths, ["x.zip"]);
        assert!(q.confirmed && q.needs_confirm);
    }

    #[test]
    fn fill_without_remote_uses_snapshot() {
        let env = Env { sneakernet: true, ..Env::new() };
        env.synced("a.zip", KB);
        env.put(env.destination.path(), "old.zip", KB, b's');
        env.record_destination();
        env.source("new.zip", 2 * KB);
        // Destination gains new.zip after the visit: only the snapshot counts, so it is still carried.
        env.put(env.destination.path(), "new.zip", 2 * KB, b'o');
        let (r, ev) = env.fill();
        assert!(!ev.iter().any(|e| matches!(e, Event::Error { .. })), "{ev:?}");
        assert_eq!((r.carried, r.carried_bytes), (1, 2 * KB as u64));
        assert_eq!(env.manifest().items.iter().map(|i| i.rel.as_str()).collect::<Vec<_>>(), ["new.zip"]);
        let q = &env.queue().pairs["p"];
        assert_eq!(q.paths, ["old.zip"], "in the snapshot, not at the office");
        assert_eq!(q.destination_files, 2, "from the snapshot, not the live home");
        assert!(env.destination.path().join("old.zip").exists());
    }

    #[test]
    fn sneakernet_fill_leaves_deletes_for_the_unload() {
        let mut env = Env { sneakernet: true, ..Env::new() };
        env.opts.delete_confirm_percent = 100;
        env.synced("a.zip", KB);
        env.put(env.destination.path(), "stale.zip", KB, b's');
        env.record_destination();
        // Caught up, under the thresholds: live, this fill would delete stale.zip itself.
        let (r, ev) = env.fill();
        assert_eq!((r.carried, r.deleted), (0, 0));
        assert!(!ev.iter().any(|e| matches!(e, Event::Deleted { .. } | Event::Error { .. })), "{ev:?}");
        assert!(env.destination.path().join("stale.zip").exists());
        let q = &env.queue().pairs["p"];
        assert_eq!(q.paths, ["stale.zip"]);
        assert!(!q.needs_confirm);
    }

    #[test]
    fn fill_without_snapshot_needs_destination_visit() {
        let env = Env { sneakernet: true, ids: &["p", "q"], ..Env::new() };
        env.source("a.zip", KB);
        env.seed_queue(&["x.zip"]);
        let (r, ev) = env.fill();
        assert_eq!(r.carried, 0);
        assert!(env.manifest().items.is_empty());
        let errors: Vec<_> = ev.iter().filter(|e| matches!(e, Event::Error { .. })).collect();
        assert_eq!(errors, [&Event::Error { pair: String::new(), msg: "Visit Lake house once so Ferry can record what's there.".into() }]);
        assert_eq!(env.queue().pairs["p"].paths, ["x.zip"], "a skipped pair's queue is left alone");
    }

    #[test]
    fn fill_without_snapshot_destination_empty_ok() {
        let mut env = Env { sneakernet: true, ..Env::new() };
        env.opts.destination_starts_empty = true;
        env.source("a.zip", KB);
        fs::create_dir(env.source.path().join("sub")).unwrap();
        env.put(&env.source.path().join("sub"), "b.zip", KB, b'o');
        env.put(env.destination.path(), "a.zip", KB, b'o'); // not recorded, so not known
        let (r, ev) = env.fill();
        assert!(!ev.iter().any(|e| matches!(e, Event::Error { .. })), "{ev:?}");
        assert_eq!(r.carried, 2);
        assert!(env.queue().pairs["p"].paths.is_empty());
        // Once a snapshot exists, the setting no longer matters.
        env.record_destination();
        let (r, _) = env.fill();
        assert_eq!(r.carried, 0);
        assert_eq!(env.queue().pairs["p"].paths, Vec::<String>::new());
    }

    #[test]
    fn damaged_snapshot_is_set_aside_and_counts_as_none() {
        let env = Env { sneakernet: true, ..Env::new() };
        env.source("a.zip", KB);
        fs::create_dir_all(env.snapshots()).unwrap();
        fs::write(env.snapshots().join("p.snap"), b"ferry-snapshot 1\nD \nF 1 2").unwrap();
        let (r, ev) = env.fill();
        assert_eq!(r.carried, 0, "not an empty home");
        assert!(ev.contains(&Event::Error { pair: String::new(), msg: "Visit Lake house once so Ferry can record what's there.".into() }));
        assert!(ev.iter().any(|e| matches!(e, Event::Info { pair, msg } if pair == "p" && msg.contains("set aside"))), "{ev:?}");
        assert!(!env.snapshots().join("p.snap").exists());
        let aside = fs::read_dir(env.snapshots()).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("p.snap.corrupt-")).count();
        assert_eq!(aside, 1);
    }

    #[test]
    fn damaged_snapshot_is_not_a_first_trip() {
        let mut env = Env { sneakernet: true, ..Env::new() };
        env.opts.destination_starts_empty = true;
        env.source("a.zip", KB);
        fs::create_dir_all(env.snapshots()).unwrap();
        fs::write(env.snapshots().join("p.snap"), b"garbage").unwrap();
        let (r, ev) = env.fill();
        assert_eq!(r.carried, 0, "a damaged snapshot never reads as an empty home");
        assert!(ev.contains(&Event::Error { pair: String::new(), msg: visit_destination("Lake house") }));
    }

    #[test]
    fn lost_snapshot_is_not_a_first_trip() {
        let mut env = Env { sneakernet: true, ..Env::new() };
        env.opts.destination_starts_empty = true;
        env.source("a.zip", KB);
        env.record_destination();
        fs::remove_file(env.snapshots().join("p.snap")).unwrap();
        let (r, ev) = env.fill();
        assert_eq!(r.carried, 0);
        assert!(ev.contains(&Event::Error { pair: String::new(), msg: visit_destination("Lake house") }));
        assert!(ev.iter().any(|e| matches!(e, Event::Info { msg, .. } if msg == "the snapshot of p at Lake house is missing")), "{ev:?}");
    }

    #[test]
    fn unreachable_destination_remote_never_falls_back_to_the_snapshot() {
        let mut env = Env::new();
        env.source("a.zip", KB);
        env.record_destination();
        fs::write(env.destination.path().join("afile"), b"x").unwrap();
        env.destination_meta = Some(env.destination.path().join("afile").join("home"));
        let (r, ev) = env.fill();
        assert_eq!(r.carried, 0);
        assert!(ev.iter().any(|e| matches!(e, Event::Error { msg, .. } if msg.starts_with("can't reach Lake house: "))), "{ev:?}");
    }
}
