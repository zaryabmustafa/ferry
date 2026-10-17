use crate::cargo::CargoManifest;
use crate::entry::{list_dir, name_order, Entry, TICKS_PER_SEC};
use crate::snapshot::SnapshotReader;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub pair: String,
    pub rel: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairDiff {
    pub carry: Vec<Candidate>,
    pub deletes: Vec<String>,
    pub cargo_gone: Vec<String>,
    /// False if any subdirectory could not be listed (its subtree was skipped).
    pub complete: bool,
    pub source_files: u64,
    pub destination_files: u64,
    /// Source files that differ from the destination and aren't in cargo, but are still inside the
    /// stability window: not candidates yet, yet they must hold the pair's deletes.
    pub unstable: u64,
}

/// SMB and the destination filesystem round mtimes differently; 2 s is the FAT-style tolerance.
pub fn same_mtime(a: i64, b: i64) -> bool {
    a.abs_diff(b) <= 2 * TICKS_PER_SEC as u64
}

pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}\\{name}")
    }
}

/// Merge order of two sorted listings. SMB names are case-insensitive, so names equal
/// ignoring case are the same file.
fn merge_order(a: &str, b: &str) -> Ordering {
    if a.to_lowercase() == b.to_lowercase() {
        Ordering::Equal
    } else {
        name_order(a, b)
    }
}

pub(crate) fn at(root: &Path, rel: &str) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

/// What the diff compares the source against (spec 4.4).
pub enum DestinationView<'a> {
    /// The destination root itself.
    Live(&'a Path),
    /// The destination snapshot: a complete listing, so a directory it lacks is empty.
    Snapshot(&'a mut SnapshotReader),
}

impl DestinationView<'_> {
    fn list(&mut self, rel: &str) -> io::Result<Vec<Entry>> {
        match self {
            DestinationView::Live(root) => list_dir(&at(root, rel)),
            DestinationView::Snapshot(r) => r.dir(rel),
        }
    }
}

/// Everything under a destination-only directory goes into `deletes`.
fn delete_tree(destination: &mut DestinationView, rel: &str, d: &mut PairDiff) {
    let mut stack = vec![rel.to_string()];
    while let Some(dir) = stack.pop() {
        match destination.list(&dir) {
            Ok(entries) => {
                for e in entries {
                    let r = join(&dir, &e.name);
                    if e.is_dir {
                        stack.push(r);
                    } else {
                        d.destination_files += 1;
                        d.deletes.push(r);
                    }
                }
            }
            Err(_) => d.complete = false,
        }
    }
}

pub fn diff_pair(
    pair: &str,
    source_root: &Path,
    mut destination: DestinationView,
    cargo: &CargoManifest,
    now: i64,
    stable_secs: i64,
) -> io::Result<PairDiff> {
    let mut d = PairDiff {
        carry: vec![],
        deletes: vec![],
        cargo_gone: vec![],
        complete: true,
        source_files: 0,
        destination_files: 0,
        unstable: 0,
    };
    // Cargo rels not yet seen at the source; leftovers are gone.
    // Keyed by lowercase rel (case-insensitive shares), valued by the original rel.
    let mut unseen: HashMap<String, &str> =
        cargo.items.iter().filter(|i| i.pair == pair).map(|i| (i.rel.to_lowercase(), i.rel.as_str())).collect();

    // (rel dir, does the destination side exist as a directory)
    let mut stack = vec![(String::new(), true)];
    while let Some((dir, destination_exists)) = stack.pop() {
        let is_root = dir.is_empty();
        let source = match list_dir(&at(source_root, &dir)) {
            Ok(v) => v,
            Err(e) if is_root => return Err(e),
            Err(_) => {
                d.complete = false;
                skip_subtree(&mut unseen, &dir);
                continue;
            }
        };
        let destination_list: Vec<Entry> = if !destination_exists {
            vec![]
        } else {
            match destination.list(&dir) {
                Ok(v) => v,
                Err(e) if is_root && e.kind() == io::ErrorKind::NotFound => vec![],
                Err(e) if is_root => return Err(e),
                Err(_) => {
                    d.complete = false;
                    skip_subtree(&mut unseen, &dir);
                    continue;
                }
            }
        };

        let (mut i, mut j) = (0, 0);
        while i < source.len() || j < destination_list.len() {
            let ord = match (source.get(i), destination_list.get(j)) {
                (Some(o), Some(h)) => merge_order(&o.name, &h.name),
                (Some(_), None) => Ordering::Less,
                _ => Ordering::Greater,
            };
            match ord {
                Ordering::Less => {
                    handle_source(pair, &dir, &source[i], None, cargo, now, stable_secs, &mut unseen, &mut stack, &mut d);
                    i += 1;
                }
                Ordering::Greater => {
                    let h = &destination_list[j];
                    let r = join(&dir, &h.name);
                    if h.is_dir {
                        delete_tree(&mut destination, &r, &mut d);
                    } else {
                        d.destination_files += 1;
                        d.deletes.push(r);
                    }
                    j += 1;
                }
                Ordering::Equal => {
                    let (o, h) = (&source[i], &destination_list[j]);
                    if h.is_dir && !o.is_dir {
                        // The source wins: the destination dir's contents go.
                        delete_tree(&mut destination, &join(&dir, &h.name), &mut d);
                        handle_source(pair, &dir, o, None, cargo, now, stable_secs, &mut unseen, &mut stack, &mut d);
                    } else if !h.is_dir && o.is_dir {
                        d.destination_files += 1;
                        d.deletes.push(join(&dir, &h.name));
                        handle_source(pair, &dir, o, None, cargo, now, stable_secs, &mut unseen, &mut stack, &mut d);
                    } else {
                        if !h.is_dir {
                            d.destination_files += 1;
                        }
                        handle_source(pair, &dir, o, Some(h), cargo, now, stable_secs, &mut unseen, &mut stack, &mut d);
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
    }
    d.cargo_gone = unseen.into_values().map(String::from).collect();
    d.cargo_gone.sort();
    Ok(d)
}

fn skip_subtree(unseen: &mut HashMap<String, &str>, dir: &str) {
    if dir.is_empty() {
        unseen.clear();
        return;
    }
    let prefix = format!("{}\\", dir.to_lowercase());
    unseen.retain(|r, _| !r.starts_with(&prefix));
}

/// Process one source entry. `destination` is the same-named destination entry when it is of the same kind.
#[allow(clippy::too_many_arguments)]
fn handle_source(
    pair: &str,
    dir: &str,
    o: &Entry,
    destination: Option<&Entry>,
    cargo: &CargoManifest,
    now: i64,
    stable_secs: i64,
    unseen: &mut HashMap<String, &str>,
    stack: &mut Vec<(String, bool)>,
    d: &mut PairDiff,
) {
    let rel = join(dir, &o.name);
    if o.is_dir {
        stack.push((rel, destination.is_some()));
        return;
    }
    d.source_files += 1;
    unseen.remove(&rel.to_lowercase());
    let differs = match destination {
        Some(h) => h.size != o.size || !same_mtime(h.mtime, o.mtime),
        None => true,
    };
    if differs && !cargo.has(pair, &rel, o.size, o.mtime) {
        if now.saturating_sub(o.mtime) >= stable_secs.saturating_mul(TICKS_PER_SEC) {
            d.carry.push(Candidate { pair: pair.to_string(), rel, size: o.size, mtime: o.mtime });
        } else {
            d.unstable += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cargo::CargoItem;
    use std::fs;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    const DAY: i64 = 86_400 * TICKS_PER_SEC;

    fn to_ticks(t: SystemTime) -> i64 {
        let s = t.duration_since(UNIX_EPOCH).unwrap();
        (s.as_secs() as i64 + EPOCH_DIFF_SECS) * TICKS_PER_SEC + (s.subsec_nanos() / 100) as i64
    }

    fn from_ticks(t: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_nanos(((t - EPOCH_DIFF_SECS * TICKS_PER_SEC) * 100) as u64)
    }

    fn now() -> i64 {
        to_ticks(SystemTime::now())
    }

    /// Write `rel` (with `\` separators) under `root` with `size` bytes and the given mtime ticks.
    fn put(root: &Path, rel: &str, size: usize, mtime: i64) {
        let p = rel.split('\\').fold(root.to_path_buf(), |a, c| a.join(c));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![b'x'; size]).unwrap();
        let f = fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(from_ticks(mtime)).unwrap();
    }

    fn rels(c: &[Candidate]) -> Vec<String> {
        let mut v: Vec<_> = c.iter().map(|c| c.rel.clone()).collect();
        v.sort();
        v
    }

    fn sorted(v: &[String]) -> Vec<String> {
        let mut v = v.to_vec();
        v.sort();
        v
    }

    fn run(source: &Path, destination: &Path, cargo: &CargoManifest) -> PairDiff {
        diff_pair("p", source, DestinationView::Live(destination), cargo, now(), 600).unwrap()
    }

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, i64) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), now() - DAY)
    }

    #[test]
    fn new_source_file_is_candidate() {
        let (o, h, t) = setup();
        put(o.path(), "a.zip", 3, t);
        let d = run(o.path(), h.path(), &CargoManifest::default());
        assert_eq!(rels(&d.carry), ["a.zip"]);
        assert_eq!((d.carry[0].pair.as_str(), d.carry[0].size, d.carry[0].mtime), ("p", 3, t));
        assert!(d.complete);
    }

    #[test]
    fn identical_file_is_skipped() {
        let (o, h, t) = setup();
        put(o.path(), "a.zip", 3, t);
        put(h.path(), "a.zip", 3, t + TICKS_PER_SEC);
        let d = run(o.path(), h.path(), &CargoManifest::default());
        assert!(d.carry.is_empty());
        assert!(d.deletes.is_empty());
    }

    #[test]
    fn mtime_beyond_tolerance_is_candidate() {
        let (o, h, t) = setup();
        put(o.path(), "a.zip", 3, t);
        put(h.path(), "a.zip", 3, t + 3 * TICKS_PER_SEC);
        let d = run(o.path(), h.path(), &CargoManifest::default());
        assert_eq!(rels(&d.carry), ["a.zip"]);
    }

    #[test]
    fn rotated_zip_gives_carry_and_delete() {
        let (o, h, t) = setup();
        put(o.path(), "x-2026-10-16.zip", 5, t);
        put(h.path(), "x-2026-10-15.zip", 5, t - DAY);
        let d = run(o.path(), h.path(), &CargoManifest::default());
        assert_eq!(rels(&d.carry), ["x-2026-10-16.zip"]);
        assert_eq!(sorted(&d.deletes), ["x-2026-10-15.zip"]);
    }

    #[test]
    fn destination_only_dir_deletes_all_files_under_it() {
        let (o, h, t) = setup();
        put(h.path(), "old\\a", 1, t);
        put(h.path(), "old\\b", 1, t);
        let d = run(o.path(), h.path(), &CargoManifest::default());
        assert_eq!(sorted(&d.deletes), ["old\\a", "old\\b"]);
    }

    #[test]
    fn file_already_in_cargo_is_not_candidate() {
        let (o, h, t) = setup();
        put(o.path(), "a.zip", 3, t);
        let cargo = CargoManifest {
            items: vec![CargoItem { pair: "p".into(), rel: "a.zip".into(), size: 3, mtime: t, hash: 0 }], ..Default::default()
        };
        let d = run(o.path(), h.path(), &cargo);
        assert!(d.carry.is_empty());
        assert!(d.cargo_gone.is_empty());
    }

    #[test]
    fn cargo_item_gone_from_source_is_reported() {
        let (o, h, t) = setup();
        put(o.path(), "kept.zip", 1, t);
        let item = |pair: &str, rel: &str| CargoItem { pair: pair.into(), rel: rel.into(), size: 1, mtime: t, hash: 0 };
        let cargo = CargoManifest {
            items: vec![item("p", "gone.zip"), item("p", "kept.zip"), item("other", "x.zip")], ..Default::default()
        };
        let d = run(o.path(), h.path(), &cargo);
        assert_eq!(d.cargo_gone, ["gone.zip"]);
    }

    #[test]
    fn recently_modified_file_is_skipped() {
        let (o, h, _) = setup();
        let n = now();
        put(o.path(), "a.zip", 3, n - 5 * 60 * TICKS_PER_SEC);
        let d = diff_pair("p", o.path(), DestinationView::Live(h.path()), &CargoManifest::default(), n, 10 * 60).unwrap();
        assert!(d.carry.is_empty());
    }

    #[test]
    fn unstable_counts_only_files_that_would_be_candidates() {
        let (o, h, t) = setup();
        let n = now();
        let fresh = n - 5 * 60 * TICKS_PER_SEC;
        put(o.path(), "new.zip", 3, fresh); // differs from the destination, too fresh: unstable
        put(o.path(), "same.zip", 3, fresh); // identical at the destination: not counted
        put(h.path(), "same.zip", 3, fresh);
        put(o.path(), "carried.zip", 3, fresh); // already in cargo: not counted
        put(o.path(), "old.zip", 3, t); // stable: a candidate
        let cargo = CargoManifest {
            items: vec![CargoItem { pair: "p".into(), rel: "carried.zip".into(), size: 3, mtime: fresh, hash: 0 }],
            ..Default::default()
        };
        let d = diff_pair("p", o.path(), DestinationView::Live(h.path()), &cargo, n, 10 * 60).unwrap();
        assert_eq!(rels(&d.carry), ["old.zip"]);
        assert_eq!(d.unstable, 1);
    }

    #[test]
    fn missing_destination_root_is_empty() {
        let (o, h, t) = setup();
        put(o.path(), "a.zip", 1, t);
        put(o.path(), "sub\\b.zip", 1, t);
        let d = run(o.path(), &h.path().join("nope"), &CargoManifest::default());
        assert_eq!(rels(&d.carry), ["a.zip", "sub\\b.zip"]);
        assert!(d.complete);
    }

    #[test]
    fn counts_files() {
        let (o, h, t) = setup();
        put(o.path(), "a", 1, t);
        put(o.path(), "d\\b", 1, t);
        put(o.path(), "d\\c", 1, t);
        put(h.path(), "a", 1, t);
        put(h.path(), "gone\\z", 1, t);
        let d = run(o.path(), h.path(), &CargoManifest::default());
        assert_eq!((d.source_files, d.destination_files), (3, 2));
    }

    #[test]
    fn destination_root_that_is_a_file_is_an_error() {
        let (o, h, t) = setup();
        put(o.path(), "a.zip", 1, t);
        let bad = h.path().join("file");
        fs::write(&bad, b"x").unwrap();
        let cargo = CargoManifest {
            items: vec![CargoItem { pair: "p".into(), rel: "a.zip".into(), size: 1, mtime: t, hash: 0 }], ..Default::default()
        };
        assert!(diff_pair("p", o.path(), DestinationView::Live(&bad), &cargo, now(), 600).is_err());
    }

    #[test]
    fn source_file_where_destination_has_dir_carries_and_deletes_contents() {
        let (o, h, t) = setup();
        put(o.path(), "x", 1, t);
        put(h.path(), "x\\y", 1, t);
        let d = run(o.path(), h.path(), &CargoManifest::default());
        assert_eq!(rels(&d.carry), ["x"]);
        assert_eq!(sorted(&d.deletes), ["x\\y"]);
    }

    #[test]
    fn source_dir_where_destination_has_file_recurses_and_deletes_file() {
        let (o, h, t) = setup();
        put(o.path(), "x\\y", 1, t);
        put(h.path(), "x", 1, t);
        let d = run(o.path(), h.path(), &CargoManifest::default());
        assert_eq!(rels(&d.carry), ["x\\y"]);
        assert_eq!(sorted(&d.deletes), ["x"]);
    }

    #[test]
    fn names_differing_only_in_case_are_the_same_file() {
        let (o, h, t) = setup();
        put(o.path(), "A.zip", 3, t);
        put(h.path(), "a.zip", 3, t);
        let d = run(o.path(), h.path(), &CargoManifest::default());
        assert!(d.carry.is_empty());
        assert!(d.deletes.is_empty());
    }

    #[test]
    fn cargo_matching_ignores_case() {
        let (o, h, t) = setup();
        put(o.path(), "A.zip", 3, t);
        let cargo = CargoManifest {
            items: vec![CargoItem { pair: "p".into(), rel: "a.ZIP".into(), size: 3, mtime: t, hash: 0 }], ..Default::default()
        };
        let d = run(o.path(), h.path(), &cargo);
        assert!(d.carry.is_empty());
        assert!(d.cargo_gone.is_empty());
    }

    #[test]
    fn diff_against_snapshot_matches_live_diff() {
        use crate::snapshot::Snapshot;
        let (o, h, t) = setup();
        put(o.path(), "same.zip", 3, t);
        put(h.path(), "same.zip", 3, t);
        put(o.path(), "Case.zip", 3, t);
        put(h.path(), "case.ZIP", 3, t);
        put(o.path(), "changed.zip", 3, t);
        put(h.path(), "changed.zip", 4, t);
        put(o.path(), "new\\deep\\n.zip", 1, t);
        put(o.path(), "carried.zip", 1, t);
        put(o.path(), "x", 1, t); // file at the source, folder at the destination
        put(h.path(), "x\\y", 1, t);
        put(o.path(), "z\\w", 1, t); // folder at the source, file at the destination
        put(h.path(), "z", 1, t);
        put(h.path(), "gone.zip", 1, t);
        put(h.path(), "old\\a", 1, t);
        put(h.path(), "old\\b\\c", 1, t);
        put(o.path(), "sub\\keep.zip", 2, t);
        put(h.path(), "sub\\keep.zip", 2, t);
        put(h.path(), "sub\\stale.zip", 2, t);
        let cargo = CargoManifest {
            items: vec![
                CargoItem { pair: "p".into(), rel: "carried.zip".into(), size: 1, mtime: t, hash: 0 },
                CargoItem { pair: "p".into(), rel: "vanished.zip".into(), size: 1, mtime: t, hash: 0 },
            ],
            ..Default::default()
        };
        let snap = h.path().parent().unwrap().join(format!("{}.snap", h.path().file_name().unwrap().to_string_lossy()));
        Snapshot::record(h.path(), &snap).unwrap();
        let n = now();
        let mut live = diff_pair("p", o.path(), DestinationView::Live(h.path()), &cargo, n, 600).unwrap();
        let mut r = Snapshot::open(&snap).unwrap();
        let mut from_snap = diff_pair("p", o.path(), DestinationView::Snapshot(&mut r), &cargo, n, 600).unwrap();
        drop(r);
        fs::remove_file(&snap).unwrap();
        for d in [&mut live, &mut from_snap] {
            d.carry.sort_by(|a, b| a.rel.cmp(&b.rel));
            d.deletes.sort();
        }
        assert_eq!(from_snap, live);
        assert_eq!(rels(&live.carry), ["changed.zip", "new\\deep\\n.zip", "x", "z\\w"]);
        assert_eq!(live.deletes, ["gone.zip", "old\\a", "old\\b\\c", "sub\\stale.zip", "x\\y", "z"]);
        assert_eq!(live.cargo_gone, ["vanished.zip"]);
        // The empty snapshot is an empty destination.
        let empty = diff_pair("p", o.path(), DestinationView::Snapshot(&mut Default::default()), &cargo, n, 600).unwrap();
        let none = diff_pair("p", o.path(), DestinationView::Live(&h.path().join("nope")), &cargo, n, 600).unwrap();
        assert_eq!(empty, none);
    }

    /// Deny listing a directory by holding it open without sharing until dropped. (A deny ACL
    /// doesn't stop an elevated admin, such as the CI runner.)
    #[cfg(windows)]
    struct Locked {
        _dir: std::fs::File,
    }
    #[cfg(windows)]
    impl Locked {
        fn new(p: PathBuf) -> Locked {
            use std::os::windows::fs::OpenOptionsExt;
            // No sharing; FILE_FLAG_BACKUP_SEMANTICS lets a directory be opened.
            Locked { _dir: std::fs::OpenOptions::new().read(true).share_mode(0).custom_flags(0x0200_0000).open(p).unwrap() }
        }
    }

    #[cfg(windows)]
    #[test]
    fn unlistable_source_subdir_marks_incomplete_and_is_skipped() {
        let (o, h, t) = setup();
        put(o.path(), "ok.zip", 1, t);
        put(o.path(), "locked\\a.zip", 1, t);
        put(h.path(), "locked\\old.zip", 1, t);
        let cargo = CargoManifest {
            items: vec![CargoItem { pair: "p".into(), rel: "locked\\a.zip".into(), size: 1, mtime: t, hash: 0 }], ..Default::default()
        };
        let _lock = Locked::new(o.path().join("locked"));
        assert!(fs::read_dir(o.path().join("locked")).is_err(), "lock did not take effect");
        let d = run(o.path(), h.path(), &cargo);
        assert!(!d.complete);
        assert_eq!(rels(&d.carry), ["ok.zip"]);
        assert!(d.deletes.is_empty());
        assert!(d.cargo_gone.is_empty());
    }
}
