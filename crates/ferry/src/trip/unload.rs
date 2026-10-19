use super::{
    load_manifest, load_queue, part_of, prune_dirs, remove, snap_path, Event, PairRoots, SnapBatch, TripCtx,
    UnloadReport, record_failed,
};
use crate::win::copy::{copy_hashed, finalize, hash_file};
use ferry_core::cargo::{CargoItem, CargoManifest};
use ferry_core::deletes::{decide, DeleteDecision};
use ferry_core::entry::long_path;
use ferry_core::snapshot::{Change, Snapshot};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;

/// `Some(true)`: `rel` is gone at the source. `Some(false)`: it is there. `None`: unknown.
/// Windows reports an unreachable host, a missing share and a missing root folder as
/// NotFound too, so a NotFound file only counts when the source root itself is listable.
fn source_absent(source_meta: &Path, rel: &str) -> Option<bool> {
    match fs::metadata(long_path(&source_meta.join(rel))) {
        Ok(_) => Some(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::metadata(long_path(source_meta)).is_ok_and(|m| m.is_dir()).then_some(true)
        }
        Err(_) => None,
    }
}

/// The source root lists but holds nothing (a missing or emptied share, as far as we can tell).
fn source_empty(source_meta: &Path) -> bool {
    fs::read_dir(long_path(source_meta)).is_ok_and(|mut d| d.next().is_none())
}

/// The end of the error for a carried file that a destination folder of the same name blocks.
pub fn folder_clash(source: &str, destination: &str) -> String {
    format!("a folder at {destination} has the same name as a file at {source}")
}

/// How one push ended.
#[derive(Debug, PartialEq, Eq)]
enum PushOutcome {
    Done,
    /// The copy read back from the destination doesn't match: retry next trip.
    DestinationMismatch,
    /// The laptop copy itself no longer matches what was carried.
    LaptopDamaged,
}

/// Push every carried file to `destination` (LAN), verified before it replaces anything; free the
/// laptop copy; then apply each caught-up pair's queued deletes on `destination`; then record each
/// pair's destination snapshot (spec 4.4).
pub fn run_unload(ctx: &TripCtx, emit: &mut dyn FnMut(Event)) -> io::Result<UnloadReport> {
    let mut snaps = SnapBatch::new(ctx.snapshots);
    let report = deliver(ctx, &mut snaps, emit);
    // However the pushes and deletes ended, what was verified at the destination goes into the snapshots.
    snaps.flush(emit);
    let mut report = report?;

    // Spec 4.4: what the destination holds now is what a source pass without a remote address diffs against.
    for roots in ctx.pairs {
        if !ctx.go_on() {
            break;
        }
        let path = snap_path(ctx.snapshots, &roots.id);
        let first = !Snapshot::recorded_before(&path);
        match fs::create_dir_all(long_path(&roots.destination)).and_then(|()| Snapshot::record(&roots.destination, &path)) {
            Ok(()) => report.first_records += u64::from(first),
            Err(e) => emit(Event::Error { pair: roots.id.clone(), msg: format!("{}{e}", record_failed(ctx.destination_name)) }),
        }
    }
    Ok(report)
}

/// The pushes, then the deletes, then one more try at the pushes a same-named destination folder
/// blocked (the deletes may have emptied it).
fn deliver(ctx: &TripCtx, snaps: &mut SnapBatch, emit: &mut dyn FnMut(Event)) -> io::Result<UnloadReport> {
    let cancelled = || ctx.cancel.load(Ordering::Relaxed);
    let mut manifest = load_manifest(ctx.manifest_path, emit)?;
    let mut report = UnloadReport::default();
    // Pairs that lost a laptop copy: those files are outstanding again until the next fill.
    let mut lost: Vec<String> = vec![];

    let items = manifest.items.clone();
    let Some(blocked) = push_items(ctx, items, false, &mut manifest, snaps, &mut report, &mut lost, emit)? else {
        return Ok(report);
    };

    let mut queue = load_queue(ctx.deletes_path, emit)?;
    for roots in ctx.pairs {
        if cancelled() {
            break;
        }
        // A file that a destination folder blocks can only land once that folder's queued deletes
        // have run, so it doesn't hold them; anything else still on the laptop does.
        if manifest.items.iter().any(|i| i.pair == roots.id && !blocked.contains(i)) {
            continue;
        }
        let Some(q) = queue.pairs.get_mut(&roots.id) else { continue };
        if q.paths.is_empty() {
            continue;
        }
        if lost.contains(&roots.id) {
            q.outstanding += 1;
        }
        // The listing guards were enforced when fill wrote the queue; only a source folder
        // that has emptied since is checked again here.
        let empty = roots.source_meta.as_deref().is_some_and(source_empty);
        let decision = decide(
            true,
            if empty { 0 } else { 1 },
            roots.allow_empty,
            q.destination_files,
            q.paths.len(),
            q.outstanding,
            0,
            q.confirmed,
            ctx.opts.delete_confirm_percent,
            ctx.opts.delete_confirm_count,
        );
        q.needs_confirm = decision == DeleteDecision::NeedsConfirm;
        match decision {
            // A source with a remote address re-checks each path. One that is set but can't be
            // reached holds them; with none set, the queue the source built is applied (spec 4.4).
            DeleteDecision::Apply if roots.source_meta.is_some() || !ctx.source_remote_configured => {
                report.deleted += apply_deletes(ctx, roots, &mut q.paths, snaps, emit);
                q.confirmed = false;
            }
            DeleteDecision::Apply => {}
            DeleteDecision::NeedsConfirm => {
                emit(Event::HeldForConfirm { pair: roots.id.clone(), count: q.paths.len() })
            }
            DeleteDecision::Hold if empty && !roots.allow_empty => {
                let pair = &roots.id;
                emit(Event::Info { pair: pair.clone(), msg: format!("folder {pair} looks empty at {}; deletes held", ctx.source_name) })
            }
            DeleteDecision::Hold => {}
        }
        queue.save(ctx.deletes_path)?;
    }
    if !blocked.is_empty() && !cancelled() {
        push_items(ctx, blocked, true, &mut manifest, snaps, &mut report, &mut lost, emit)?;
    }
    Ok(report)
}

/// Push each item, or discard it when it's gone at the source, and free its laptop copy. Returns
/// the items a same-named destination folder blocks (with an error for each on the `last_try`),
/// or `None` when the trip stopped.
#[allow(clippy::too_many_arguments)]
fn push_items(
    ctx: &TripCtx,
    items: Vec<CargoItem>,
    last_try: bool,
    manifest: &mut CargoManifest,
    snaps: &mut SnapBatch,
    report: &mut UnloadReport,
    lost: &mut Vec<String>,
    emit: &mut dyn FnMut(Event),
) -> io::Result<Option<Vec<CargoItem>>> {
    let cancelled = || ctx.cancel.load(Ordering::Relaxed);
    let mut blocked = vec![];
    for item in items {
        if !ctx.go_on() {
            return Ok(None);
        }
        let Some(roots) = ctx.pairs.iter().find(|r| r.id == item.pair) else { continue };
        let err = |msg: String| Event::Error { pair: item.pair.clone(), msg };
        let laptop = ctx.cargo_dir.join(&item.pair).join(&item.rel);
        if matches!(fs::metadata(long_path(&laptop)), Err(e) if e.kind() == io::ErrorKind::NotFound) {
            // E.g. a crash between freeing the copy and saving the manifest. Drop the entry
            // so it can't block the pair forever; the next fill carries it again if needed.
            manifest.remove(&item.pair, &item.rel);
            manifest.save(ctx.manifest_path)?;
            lost.push(item.pair.clone());
            emit(err(format!("{}: laptop copy missing, dropped from cargo", item.rel)));
            continue;
        }
        // Only a definite absence discards; source unreachable or unknown pushes.
        let at_source_gone = roots.source_meta.as_ref().and_then(|m| source_absent(m, &item.rel)) == Some(true);
        let dst = roots.destination.join(&item.rel);
        if !at_source_gone && fs::metadata(long_path(&dst)).is_ok_and(|m| m.is_dir()) && !remove_empty_tree(&dst) {
            // A folder with files in it can't be replaced by a file. Its queued deletes may clear
            // it this trip; otherwise the pair waits until someone sorts it out at the destination.
            if last_try {
                emit(err(format!("{}: {}", item.rel, folder_clash(ctx.source_name, ctx.destination_name))));
            }
            blocked.push(item);
            continue;
        }
        if !at_source_gone {
            match push(ctx, &roots.destination, &item, &laptop, emit) {
                Ok(PushOutcome::Done) => {
                    let change = Change::Push { rel: item.rel.clone(), size: item.size, mtime: item.mtime };
                    snaps.add(&item.pair, change, emit);
                    report.pushed += 1;
                    report.pushed_bytes += item.size;
                    emit(Event::Pushed { pair: item.pair.clone(), rel: item.rel.clone(), size: item.size });
                }
                Ok(PushOutcome::DestinationMismatch) => {
                    report.failed += 1;
                    emit(Event::VerifyFailed { pair: item.pair.clone(), rel: item.rel.clone() });
                    continue;
                }
                Ok(PushOutcome::LaptopDamaged) => {
                    // Ruling R32: retrying can't help. Drop it; the next fill carries it again.
                    if let Err(e) = remove(&laptop) {
                        emit(err(format!("{}: {e}", item.rel)));
                        continue;
                    }
                    manifest.remove(&item.pair, &item.rel);
                    manifest.save(ctx.manifest_path)?;
                    lost.push(item.pair.clone());
                    emit(err(format!("{}: laptop copy was damaged; will carry it again", item.rel)));
                    continue;
                }
                Err(_) if cancelled() => return Ok(None),
                Err(e) => {
                    report.failed += 1;
                    emit(err(format!("{}: {e}", item.rel)));
                    continue;
                }
            }
        }
        // Pushed or discarded: free the laptop copy. If that fails the entry stays, so the
        // next unload pushes it again (harmless) rather than orphaning the file.
        match remove(&laptop) {
            Ok(()) => {
                manifest.remove(&item.pair, &item.rel);
                manifest.save(ctx.manifest_path)?;
                if at_source_gone {
                    report.discarded += 1;
                }
            }
            Err(e) => emit(err(format!("{}: {e}", item.rel))),
        }
    }
    snaps.flush(emit);
    Ok(Some(blocked))
}

/// Remove `dir` if no file is anywhere inside it (empty folders hold no data). True if it's gone.
fn remove_empty_tree(dir: &Path) -> bool {
    let only_empty_dirs = fs::read_dir(long_path(dir))
        .is_ok_and(|mut d| d.all(|e| e.is_ok_and(|e| e.file_type().is_ok_and(|t| t.is_dir()) && remove_empty_tree(&e.path()))));
    only_empty_dirs && fs::remove_dir(long_path(dir)).is_ok()
}

/// Delete each queued path at the destination that is still gone at the source (without `source_meta`,
/// as queued); one that is back at the source is dropped. Paths whose delete failed or whose
/// source state is unknown stay queued.
fn apply_deletes(ctx: &TripCtx, roots: &PairRoots, paths: &mut Vec<String>, snaps: &mut SnapBatch, emit: &mut dyn FnMut(Event)) -> u64 {
    let mut deleted = 0;
    let mut keep = vec![];
    let mut unknown = 0;
    for rel in std::mem::take(paths) {
        let absent = roots.source_meta.as_deref().map_or(Some(true), |m| source_absent(m, &rel));
        match absent {
            Some(false) => {}
            Some(true) => match remove(&roots.destination.join(&rel)) {
                Ok(()) => {
                    prune_dirs(&roots.destination, &roots.destination.join(&rel));
                    snaps.add(&roots.id, Change::Delete { rel: rel.clone() }, emit);
                    deleted += 1;
                    emit(Event::Deleted { pair: roots.id.clone(), rel });
                }
                Err(e) => {
                    emit(Event::Error { pair: roots.id.clone(), msg: format!("delete {rel}: {e}") });
                    keep.push(rel);
                }
            },
            None => {
                unknown += 1;
                keep.push(rel);
            }
        }
    }
    if unknown > 0 {
        emit(Event::Error { pair: roots.id.clone(), msg: format!("can't reach {}: {unknown} deletes held", ctx.source_name) });
    }
    *paths = keep;
    deleted
}

/// Fresh push to `<destination>/<rel>.ferry-part`, verified by reading the part back before it
/// replaces the destination file. The hash taken while reading the laptop copy tells a damaged laptop
/// copy from a bad copy at the destination. Any outcome but `Done` removes the part.
fn push(ctx: &TripCtx, destination: &Path, item: &CargoItem, laptop: &Path, emit: &mut dyn FnMut(Event)) -> io::Result<PushOutcome> {
    let dst = destination.join(&item.rel);
    let part = part_of(&dst);
    // No resume over LAN: a leftover part is from an earlier, unverified push.
    remove(&part)?;
    if let Some(dir) = dst.parent() {
        fs::create_dir_all(long_path(dir))?;
    }
    let r = copy_hashed(laptop, &part, ctx.cancel, &mut |done| {
        emit(Event::Progress { pair: item.pair.clone(), rel: item.rel.clone(), done, total: item.size });
        ctx.go_on(); // spec 4.3: a path that stops being allowed cancels the push
    })
    .and_then(|c| match c.hash == item.hash {
        false => Ok(PushOutcome::LaptopDamaged),
        // The read-back crosses the network too: a lost path stops it like a cancelled push.
        true if !ctx.go_on() => Err(io::ErrorKind::Interrupted.into()),
        true if hash_file(&part, ctx.cancel, &mut |_| {
            ctx.go_on();
        })? != item.hash => Ok(PushOutcome::DestinationMismatch),
        true => finalize(&part, &dst, item.mtime).map(|()| PushOutcome::Done),
    });
    if !matches!(r, Ok(PushOutcome::Done)) {
        // Best effort: must not mask the push's own result.
        let _ = remove(&part);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trip::fill::run_fill;
    use crate::trip::{FillReport, PairRoots};
    use ferry_core::cargo::CargoManifest;
    use ferry_core::config::{Options, GB};
    use ferry_core::deletes::DeleteQueue;
    use std::fs;
    use std::os::windows::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    const KB: usize = 1024;

    struct Env {
        /// LAN and Tailscale roots are the same dir in tests.
        source: TempDir,
        destination: TempDir,
        laptop: TempDir,
        t: SystemTime,
        opts: Options,
        /// Remote view of the source; `None` = unreachable or no remote address.
        source_meta: Option<PathBuf>,
        /// The source has a remote address (so a missing `source_meta` means unreachable).
        source_remote: bool,
        /// No remote address for the destination: fills diff against the destination snapshot.
        sneakernet: bool,
    }

    impl Env {
        fn new() -> Env {
            let source = tempfile::tempdir().unwrap();
            let source_meta = Some(source.path().into());
            Env {
                source,
                destination: tempfile::tempdir().unwrap(),
                laptop: tempfile::tempdir().unwrap(),
                t: SystemTime::now() - Duration::from_secs(86_400),
                opts: Options { cap_bytes: GB, min_free_bytes: 0, ..Options::default() },
                source_meta,
                source_remote: true,
                sneakernet: false,
            }
        }
        /// Neither server has a remote address.
        fn sneakernet() -> Env {
            Env { source_meta: None, source_remote: false, sneakernet: true, ..Env::new() }
        }
        /// The destination snapshot's top-level names, or `None` without one.
        fn snapshot(&self) -> Option<Vec<String>> {
            let mut r = ferry_core::snapshot::Snapshot::open(&self.laptop.path().join("snapshots").join("p.snap")).ok()?;
            Some(r.dir("").unwrap().into_iter().map(|e| e.name).collect())
        }
        fn put_at(&self, root: &Path, name: &str, len: usize, fill: u8, t: SystemTime) -> PathBuf {
            let p = root.join(name);
            fs::write(&p, vec![fill; len]).unwrap();
            fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(t).unwrap();
            p
        }
        fn source(&self, name: &str, len: usize) -> PathBuf {
            self.put_at(self.source.path(), name, len, b'o', self.t)
        }
        fn destination(&self, name: &str) -> PathBuf {
            self.destination.path().join(name)
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
        fn manifest(&self) -> CargoManifest {
            CargoManifest::load(&self.manifest_path()).unwrap()
        }
        fn queued(&self) -> Vec<String> {
            DeleteQueue::load(&self.deletes_path()).unwrap().pairs["p"].paths.clone()
        }
        fn run<R>(&self, trip: fn(&TripCtx, &mut dyn FnMut(Event)) -> io::Result<R>) -> (R, Vec<Event>) {
            let mut events = vec![];
            let r = self.run_with(trip, &AtomicBool::new(false), &mut |e| events.push(e));
            (r, events)
        }
        fn run_with<R>(
            &self,
            trip: fn(&TripCtx, &mut dyn FnMut(Event)) -> io::Result<R>,
            cancel: &AtomicBool,
            emit: &mut dyn FnMut(Event),
        ) -> R {
            self.run_path(trip, cancel, &|| true, emit)
        }
        fn run_path<R>(
            &self,
            trip: fn(&TripCtx, &mut dyn FnMut(Event)) -> io::Result<R>,
            cancel: &AtomicBool,
            path_ok: &dyn Fn() -> bool,
            emit: &mut dyn FnMut(Event),
        ) -> R {
            let pairs = [PairRoots {
                id: "p".into(),
                source: self.source.path().into(),
                source_meta: self.source_meta.clone(),
                destination: self.destination.path().into(),
                destination_meta: (!self.sneakernet).then(|| self.destination.path().into()),
                allow_empty: false,
            }];
            let (cargo, mp, dp, snaps) = (self.cargo(), self.manifest_path(), self.deletes_path(), self.laptop.path().join("snapshots"));
            let ctx = TripCtx {
                pairs: &pairs,
                cargo_dir: &cargo,
                manifest_path: &mp,
                deletes_path: &dp,
                snapshots: &snaps,
                source_remote_configured: self.source_remote,
                destination_remote_configured: !self.sneakernet,
                source_name: "HQ",
                destination_name: "Lake house",
                opts: &self.opts,
                disk_free: &|| u64::MAX,
                now: i64::MAX,
                cancel,
                path_ok,
            };
            trip(&ctx, emit).unwrap()
        }
        fn fill(&self) -> FillReport {
            self.run(run_fill).0
        }
        fn unload(&self) -> (UnloadReport, Vec<Event>) {
            self.run(run_unload)
        }
    }

    fn mtime(p: &Path) -> u64 {
        fs::metadata(p).unwrap().last_write_time()
    }

    #[test]
    fn pushes_verifies_and_frees_laptop() {
        let env = Env::new();
        env.source("a.zip", 4 * KB);
        env.source("b.zip", 8 * KB);
        assert_eq!(env.fill().carried, 2);
        let (r, ev) = env.unload();
        assert_eq!((r.pushed, r.pushed_bytes, r.failed), (2, 12 * KB as u64, 0));
        assert_eq!(fs::read(env.destination("a.zip")).unwrap(), vec![b'o'; 4 * KB]);
        assert_eq!(fs::read(env.destination("b.zip")).unwrap(), vec![b'o'; 8 * KB]);
        assert!(ev.contains(&Event::Pushed { pair: "p".into(), rel: "a.zip".into(), size: 4 * KB as u64 }));
        assert_eq!(fs::read_dir(env.cargo().join("p")).unwrap().count(), 0);
        assert!(env.manifest().items.is_empty());
        assert!(!env.destination("a.zip.ferry-part").exists());
    }

    #[test]
    fn sets_destination_mtime_to_source_mtime() {
        let env = Env::new();
        let src = env.source("a.zip", 4 * KB);
        env.fill();
        env.unload();
        assert_eq!(mtime(&env.destination("a.zip")), mtime(&src));
        assert_eq!(env.fill().carried, 0);
    }

    #[test]
    fn discards_items_deleted_at_source() {
        let env = Env::new();
        let src = env.source("a.zip", 4 * KB);
        env.fill();
        fs::remove_file(src).unwrap();
        let (r, ev) = env.unload();
        assert_eq!((r.pushed, r.discarded), (0, 1));
        assert!(!ev.iter().any(|e| matches!(e, Event::Pushed { .. })));
        assert!(!env.destination("a.zip").exists());
        assert!(!env.cargo().join("p").join("a.zip").exists());
        assert!(env.manifest().items.is_empty());
    }

    #[test]
    fn pushes_when_source_unreachable() {
        let mut env = Env::new();
        let src = env.source("a.zip", 4 * KB);
        env.fill();
        // Gone at the source, but with no Tailscale view of the source it can't be known.
        fs::remove_file(src).unwrap();
        env.source_meta = None;
        let (r, _) = env.unload();
        assert_eq!((r.pushed, r.discarded), (1, 0));
        assert_eq!(fs::read(env.destination("a.zip")).unwrap(), vec![b'o'; 4 * KB]);
    }

    #[test]
    fn verify_mismatch_keeps_laptop_copy() {
        let env = Env::new();
        // Big enough for several progress callbacks: the first one damages the copy at the destination.
        env.source("a.zip", 3 << 20);
        let older = env.put_at(env.destination.path(), "a.zip", 2 * KB, b'h', env.t - Duration::from_secs(3600));
        let older_mtime = mtime(&older);
        env.fill();
        let part = env.destination("a.zip.ferry-part");
        let mut hit = false;
        let mut ev = vec![];
        let r = env.run_with(run_unload, &AtomicBool::new(false), &mut |e| {
            if matches!(e, Event::Progress { .. }) && !hit && part.exists() {
                hit = true;
                let f = fs::OpenOptions::new().write(true).open(&part).unwrap();
                std::os::windows::fs::FileExt::seek_write(&f, b"X", 0).unwrap();
                f.sync_all().unwrap();
            }
            ev.push(e);
        });
        assert!(hit, "no progress while the part existed");
        assert_eq!(r.pushed, 0);
        assert!(ev.contains(&Event::VerifyFailed { pair: "p".into(), rel: "a.zip".into() }));
        assert_eq!(fs::read(env.cargo().join("p").join("a.zip")).unwrap(), vec![b'o'; 3 << 20]);
        assert_eq!(env.manifest().items.len(), 1);
        assert!(!env.destination("a.zip.ferry-part").exists());
        assert_eq!(fs::read(&older).unwrap(), vec![b'h'; 2 * KB]);
        assert_eq!(mtime(&older), older_mtime);
    }

    #[test]
    fn damaged_laptop_copy_is_dropped_and_carried_again() {
        let mut env = Env::new();
        rotated(&mut env);
        let older = env.put_at(env.destination.path(), "new.zip", 2 * KB, b'h', env.t - Duration::from_secs(3600));
        let laptop = env.cargo().join("p").join("new.zip");
        let mut bytes = fs::read(&laptop).unwrap();
        bytes[1000] ^= 0xFF;
        fs::write(&laptop, bytes).unwrap();
        let (r, ev) = env.unload();
        assert_eq!((r.pushed, r.failed, r.deleted), (0, 0, 0));
        assert!(!ev.iter().any(|e| matches!(e, Event::VerifyFailed { .. })), "no copy-check toast");
        assert!(ev.iter().any(|e| matches!(e, Event::Error { msg, .. } if msg.contains("laptop copy was damaged; will carry it again"))));
        assert!(env.manifest().items.is_empty());
        assert!(!laptop.exists());
        assert!(!env.destination("new.zip.ferry-part").exists());
        assert_eq!(fs::read(&older).unwrap(), vec![b'h'; 2 * KB], "home is untouched");
        // new.zip never reached the destination, so old.zip's delete still waits.
        assert!(env.destination("old.zip").exists());
        assert_eq!(env.queued(), ["old.zip"]);
        // The next fill carries it again, and the unload after that delivers it.
        assert_eq!(env.fill().carried, 1);
        assert_eq!(env.unload().0.pushed, 1);
        assert_eq!(fs::read(env.destination("new.zip")).unwrap(), vec![b'o'; 4 * KB]);
    }

    #[test]
    fn destination_folder_named_like_the_file_is_skipped_with_one_error() {
        let env = Env::new();
        env.source("a.zip", 4 * KB);
        env.fill();
        // Made after the fill, so no deletes are queued for what's in it.
        fs::create_dir(env.destination("a.zip")).unwrap();
        env.put_at(&env.destination("a.zip"), "keep.txt", KB, b'k', env.t);
        let (r, ev) = env.unload();
        assert_eq!((r.pushed, r.failed), (0, 0));
        let errors: Vec<_> = ev.iter().filter(|e| matches!(e, Event::Error { .. })).collect();
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], Event::Error { msg, .. } if msg.ends_with("a folder at Lake house has the same name as a file at HQ")));
        assert!(!ev.iter().any(|e| matches!(e, Event::Progress { .. })), "no copy was attempted");
        assert_eq!(env.manifest().items.len(), 1);
    }

    #[test]
    fn an_empty_destination_folder_in_the_way_is_replaced() {
        let env = Env::new();
        env.source("a.zip", 4 * KB);
        env.fill();
        fs::create_dir_all(env.destination("a.zip").join("empty").join("deeper")).unwrap();
        let (r, ev) = env.unload();
        assert_eq!(r.pushed, 1);
        assert!(env.destination("a.zip").is_file());
        assert!(!ev.iter().any(|e| matches!(e, Event::Error { .. })));
    }

    #[test]
    fn a_folder_replaced_by_a_file_is_emptied_then_the_file_lands() {
        let mut env = Env::new();
        env.opts.delete_confirm_percent = 100;
        // The destination still has the folder "a.zip" the source turned into a file.
        fs::create_dir(env.destination("a.zip")).unwrap();
        env.put_at(&env.destination("a.zip"), "old.txt", KB, b'o', env.t);
        env.source("a.zip", 4 * KB);
        assert_eq!(env.fill().carried, 1);
        assert_eq!(env.queued(), [r"a.zip\old.txt"]);
        let (r, ev) = env.unload();
        assert_eq!((r.deleted, r.pushed), (1, 1));
        assert!(env.destination("a.zip").is_file());
        assert!(env.manifest().items.is_empty());
        assert!(!ev.iter().any(|e| matches!(e, Event::Error { .. })), "{ev:?}");
    }

    #[test]
    fn emptied_source_holds_deletes_with_a_note() {
        let mut env = Env::new();
        rotated(&mut env);
        // Both source files vanish (a share emptied): the push still happens, deletes wait.
        fs::remove_file(env.source.path().join("new.zip")).unwrap();
        let (r, ev) = env.unload();
        assert_eq!((r.discarded, r.deleted), (1, 0));
        assert!(env.destination("old.zip").exists());
        assert_eq!(env.queued(), ["old.zip"]);
        let notes: Vec<_> = ev.iter().filter(|e| matches!(e, Event::Info { .. })).collect();
        assert_eq!(notes, [&Event::Info { pair: "p".into(), msg: "folder p looks empty at HQ; deletes held".into() }]);
    }

    #[test]
    fn applied_deletes_remove_emptied_destination_folders() {
        let mut env = Env::new();
        env.opts.delete_confirm_percent = 100;
        env.source("new.zip", 4 * KB);
        fs::create_dir_all(env.destination.path().join("gone").join("deep")).unwrap();
        env.put_at(&env.destination.path().join("gone").join("deep"), "old.zip", KB, b's', env.t);
        assert_eq!(env.fill().carried, 1);
        assert_eq!(env.unload().0.deleted, 1);
        assert!(!env.destination("gone").exists());
        assert!(env.destination("new.zip").exists());
    }

    #[test]
    fn corrupt_state_files_do_not_block_the_unload() {
        let env = Env::new();
        env.source("a.zip", 4 * KB);
        env.fill();
        fs::write(env.manifest_path(), b"{\"items\": [").unwrap();
        fs::write(env.deletes_path(), b"garbage").unwrap();
        let (r, ev) = env.unload();
        assert_eq!(r.pushed, 0, "the cargo list is gone, so nothing is known to push");
        let resets: Vec<bool> = ev.iter().filter_map(|e| match e { Event::StateReset { manifest, .. } => Some(*manifest), _ => None }).collect();
        assert_eq!(resets, [true, false]);
        // The laptop file stays; the next fill's copy replaces it.
        assert!(env.cargo().join("p").join("a.zip").exists());
        assert_eq!(env.fill().carried, 1);
        assert_eq!(env.unload().0.pushed, 1);
    }

    #[test]
    fn held_deletes_apply_after_unload() {
        let mut env = Env::new();
        // 1 of 1 destination files is over the default 10% confirm threshold; this test is about timing.
        env.opts.delete_confirm_percent = 100;
        env.source("new.zip", 4 * KB);
        env.put_at(env.destination.path(), "old.zip", 4 * KB, b's', env.t);
        assert_eq!(env.fill().carried, 1);
        assert!(env.destination("old.zip").exists(), "fill must hold the delete while new.zip is in cargo");
        let (r, ev) = env.unload();
        assert_eq!((r.pushed, r.deleted), (1, 1));
        assert!(env.destination("new.zip").exists());
        assert!(!env.destination("old.zip").exists());
        assert!(ev.contains(&Event::Deleted { pair: "p".into(), rel: "old.zip".into() }));
        assert!(env.queued().is_empty());
    }

    #[test]
    fn held_delete_dropped_when_back_at_source() {
        let mut env = Env::new();
        env.opts.delete_confirm_percent = 100;
        env.source("new.zip", 4 * KB);
        env.put_at(env.destination.path(), "old.zip", 4 * KB, b's', env.t);
        env.fill();
        // old.zip reappears at the source before unload: it must not be deleted at the destination.
        env.source("old.zip", 4 * KB);
        let (r, _) = env.unload();
        assert_eq!((r.pushed, r.deleted), (1, 0));
        assert!(env.destination("old.zip").exists());
        assert!(env.queued().is_empty());
    }

    /// A fill that leaves new.zip in cargo and holds the delete of destination old.zip.
    fn rotated(env: &mut Env) {
        env.opts.delete_confirm_percent = 100;
        env.source("new.zip", 4 * KB);
        env.put_at(env.destination.path(), "old.zip", 4 * KB, b's', env.t);
        assert_eq!(env.fill().carried, 1);
        assert_eq!(env.queued(), ["old.zip"]);
    }

    #[test]
    fn missing_source_root_pushes_and_holds_deletes() {
        let mut env = Env::new();
        rotated(&mut env);
        // A renamed share or missing root reads as NotFound for every file under it.
        env.source_meta = Some(env.source.path().join("missing"));
        let (r, ev) = env.unload();
        assert_eq!((r.pushed, r.discarded, r.deleted), (1, 0, 0));
        assert!(env.destination("new.zip").exists());
        assert!(env.destination("old.zip").exists());
        assert_eq!(env.queued(), ["old.zip"]);
        assert_eq!(ev.iter().filter(|e| matches!(e, Event::Error { .. })).count(), 1);
    }

    #[test]
    fn deletes_held_when_source_unreachable() {
        let mut env = Env::new();
        rotated(&mut env);
        env.source_meta = None;
        let (r, _) = env.unload();
        assert_eq!((r.pushed, r.deleted), (1, 0));
        assert!(env.destination("old.zip").exists());
        assert_eq!(env.queued(), ["old.zip"]);
    }

    #[test]
    fn missing_laptop_copy_is_dropped() {
        let mut env = Env::new();
        rotated(&mut env);
        fs::remove_file(env.cargo().join("p").join("new.zip")).unwrap();
        let (r, ev) = env.unload();
        assert_eq!((r.pushed, r.failed, r.deleted), (0, 0, 0));
        assert!(env.manifest().items.is_empty());
        assert!(ev.iter().any(|e| matches!(e, Event::Error { .. })));
        // new.zip never reached the destination, so old.zip's delete still waits.
        assert!(env.destination("old.zip").exists());
        assert_eq!(env.queued(), ["old.zip"]);
        // The next fill carries it again.
        assert_eq!(env.fill().carried, 1);
    }

    #[test]
    fn cancel_during_push_removes_destination_part() {
        let env = Env::new();
        env.source("big.bin", 3 << 20);
        env.fill();
        let cancel = AtomicBool::new(false);
        let r = env.run_with(run_unload, &cancel, &mut |e| {
            if matches!(e, Event::Progress { .. }) {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!((r.pushed, r.failed), (0, 0));
        assert!(!env.destination("big.bin.ferry-part").exists());
        assert!(!env.destination("big.bin").exists());
        assert!(env.cargo().join("p").join("big.bin").exists());
        assert_eq!(env.manifest().items.len(), 1);
    }

    #[test]
    fn lost_path_stops_the_unload_and_keeps_the_laptop_copy() {
        let env = Env::new();
        env.source("big.bin", 3 << 20);
        env.fill();
        let cancel = AtomicBool::new(false);
        let lost = std::cell::Cell::new(false);
        let r = env.run_path(run_unload, &cancel, &|| !lost.get(), &mut |e| {
            if matches!(e, Event::Progress { .. }) {
                lost.set(true);
            }
        });
        assert_eq!((r.pushed, r.failed), (0, 0));
        assert!(cancel.load(Ordering::Relaxed));
        assert!(!env.destination("big.bin.ferry-part").exists() && !env.destination("big.bin").exists());
        assert_eq!(env.manifest().items.len(), 1);

        // Blocked before the next file: nothing is even started.
        let cancel = AtomicBool::new(false);
        let mut events = vec![];
        let r = env.run_path(run_unload, &cancel, &|| false, &mut |e| events.push(e));
        assert_eq!(r.pushed, 0);
        assert!(!events.iter().any(|e| matches!(e, Event::Progress { .. })));
        assert!(env.cargo().join("p").join("big.bin").exists());
    }

    #[test]
    fn lost_path_during_verify_is_a_cancelled_push() {
        // The copy's last progress tick checks the path once (call 1); then the check before
        // the verify (call 2); then the verify's own progress (call 3 on).
        for lose_at in [2, 3] {
            let env = Env::new();
            env.source("big.bin", 3 << 20);
            env.fill();
            let (copied, calls) = (std::cell::Cell::new(false), std::cell::Cell::new(0));
            let path_ok = || {
                if copied.get() {
                    calls.set(calls.get() + 1);
                }
                calls.get() < lose_at
            };
            let cancel = AtomicBool::new(false);
            let mut events = vec![];
            let r = env.run_path(run_unload, &cancel, &path_ok, &mut |e| {
                if matches!(e, Event::Progress { done, total, .. } if done == total) {
                    copied.set(true);
                }
                events.push(e);
            });
            assert!(copied.get(), "lose_at {lose_at}: the copy finished");
            assert_eq!((r.pushed, r.failed), (0, 0), "lose_at {lose_at}");
            let pushed = events.iter().any(|e| matches!(e, Event::Pushed { .. } | Event::VerifyFailed { .. }));
            assert!(!pushed, "lose_at {lose_at}");
            assert!(env.cargo().join("p").join("big.bin").exists(), "lose_at {lose_at}");
            assert_eq!(env.manifest().items.len(), 1, "lose_at {lose_at}");
            assert!(!env.destination("big.bin.ferry-part").exists() && !env.destination("big.bin").exists(), "lose_at {lose_at}");
        }
    }

    #[test]
    fn stale_destination_part_is_not_resumed() {
        let env = Env::new();
        env.source("a.zip", 8 * KB);
        env.fill();
        // Resuming this would keep its bytes and fail verify.
        fs::write(env.destination("a.zip.ferry-part"), vec![b'x'; 8 * KB]).unwrap();
        let (r, _) = env.unload();
        assert_eq!(r.pushed, 1);
        assert_eq!(fs::read(env.destination("a.zip")).unwrap(), vec![b'o'; 8 * KB]);
        assert!(!env.destination("a.zip.ferry-part").exists());
    }

    #[test]
    fn copy_error_keeps_laptop_copy() {
        let env = Env::new();
        fs::create_dir(env.source.path().join("d")).unwrap();
        env.source("d/a.zip", 4 * KB);
        env.fill();
        // A file where the destination folder should be: the push can't create its parent.
        fs::write(env.destination("d"), b"x").unwrap();
        let (r, ev) = env.unload();
        assert_eq!((r.pushed, r.failed), (0, 1));
        assert!(ev.iter().any(|e| matches!(e, Event::Error { .. })));
        assert!(env.cargo().join("p").join("d").join("a.zip").exists());
        assert_eq!(env.manifest().items.len(), 1);
    }

    #[test]
    fn unload_records_snapshot_and_applies_queue_without_source_remote() {
        let mut env = Env::sneakernet();
        env.opts.delete_confirm_percent = 100;
        env.put_at(env.destination.path(), "old.zip", KB, b's', env.t);
        assert_eq!(env.snapshot(), None);
        // First visit to the destination: nothing to push, but the destination is recorded.
        let (r, ev) = env.unload();
        assert_eq!((r.pushed, r.deleted), (0, 0));
        assert!(!ev.iter().any(|e| matches!(e, Event::Error { .. })), "{ev:?}");
        assert_eq!(env.snapshot().unwrap(), ["old.zip"]);
        // At the source: new.zip is carried, old.zip queued from the snapshot.
        env.source("new.zip", 4 * KB);
        assert_eq!(env.fill().carried, 1);
        assert_eq!(env.queued(), ["old.zip"]);
        // Destination again, no view of the source: the queue is applied as built at the source.
        let (r, ev) = env.unload();
        assert!(!ev.iter().any(|e| matches!(e, Event::Error { .. })), "{ev:?}");
        assert_eq!((r.pushed, r.deleted), (1, 1));
        assert!(env.destination("new.zip").exists());
        assert!(!env.destination("old.zip").exists());
        assert!(env.queued().is_empty());
        assert_eq!(env.snapshot().unwrap(), ["new.zip"]);
        // Destination matches the source: the next source pass has nothing to do.
        assert_eq!(env.fill().carried, 0);
        assert!(env.queued().is_empty());
    }

    #[test]
    fn unload_holds_deletes_when_source_remote_configured_but_down() {
        let mut env = Env::sneakernet();
        env.source_remote = true; // set, but not reachable from the destination
        env.opts.delete_confirm_percent = 100;
        env.put_at(env.destination.path(), "old.zip", KB, b's', env.t);
        env.unload();
        env.source("new.zip", 4 * KB);
        assert_eq!(env.fill().carried, 1);
        let (r, _) = env.unload();
        assert_eq!((r.pushed, r.deleted), (1, 0));
        assert!(env.destination("old.zip").exists());
        assert_eq!(env.queued(), ["old.zip"]);
        assert_eq!(env.snapshot().unwrap(), ["new.zip", "old.zip"], "home is still recorded");
    }

    #[test]
    fn sneakernet_deletes_keep_the_confirm_threshold() {
        let env = Env::sneakernet();
        env.source("keep.zip", KB);
        env.put_at(env.destination.path(), "keep.zip", KB, b'o', env.t);
        for i in 0..60 {
            env.put_at(env.destination.path(), &format!("gone-{i:02}.zip"), KB, b's', env.t);
        }
        env.unload();
        env.fill();
        let (r, ev) = env.unload();
        assert_eq!(r.deleted, 0);
        assert!(ev.contains(&Event::HeldForConfirm { pair: "p".into(), count: 60 }));
        assert_eq!(fs::read_dir(env.destination.path()).unwrap().count(), 61);
    }

    #[test]
    fn destination_only_file_is_removed_next_round() {
        let mut env = Env::sneakernet();
        env.opts.delete_confirm_percent = 100;
        env.source("a.zip", KB);
        env.put_at(env.destination.path(), "a.zip", KB, b'o', env.t);
        // Added by hand at the destination: the destination visit records it.
        env.put_at(env.destination.path(), "extra.zip", KB, b'h', env.t);
        env.unload();
        assert_eq!(env.snapshot().unwrap(), ["a.zip", "extra.zip"]);
        assert_eq!(env.fill().carried, 0);
        assert_eq!(env.queued(), ["extra.zip"]);
        let (r, _) = env.unload();
        assert_eq!(r.deleted, 1);
        assert!(!env.destination("extra.zip").exists());
        assert_eq!(env.snapshot().unwrap(), ["a.zip"]);
    }

    #[test]
    fn interrupted_unload_keeps_verified_pushes_in_the_snapshot() {
        let env = Env::sneakernet();
        env.unload(); // records the empty destination
        env.source("a.zip", 4 * KB);
        env.source("b.zip", 4 * KB);
        assert_eq!(env.fill().carried, 2);
        let pushed = std::cell::Cell::new(false);
        let r = env.run_path(run_unload, &AtomicBool::new(false), &|| !pushed.get(), &mut |e| {
            if matches!(e, Event::Pushed { .. }) {
                pushed.set(true); // the path is lost after the first verified push
            }
        });
        assert_eq!(r.pushed, 1);
        assert_eq!(env.manifest().items.len(), 1);
        let snap = env.snapshot().unwrap();
        assert_eq!(snap.len(), 1, "only the verified push, not what's still carried: {snap:?}");
        assert!(env.destination(&snap[0]).exists());
        // The next source pass carries nothing again: one file is at the destination, the other on the laptop.
        assert_eq!(env.fill().carried, 0);
    }
}
