use crate::cargo::{load_json, save_json};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairDeletes {
    pub paths: Vec<String>,
    pub outstanding: u64,
    pub needs_confirm: bool,
    pub confirmed: bool,
    /// Destination file count from the trusted listing that wrote `paths` (for the confirm threshold).
    #[serde(default, alias = "home_files")]
    pub destination_files: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteQueue {
    pub pairs: BTreeMap<String, PairDeletes>,
}

impl DeleteQueue {
    /// Missing file → empty queue.
    pub fn load(path: &Path) -> io::Result<Self> {
        load_json(path)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        save_json(path, self)
    }
}

/// A fingerprint of a delete list (FNV-1a over the paths, in order), so a confirmation can name
/// the exact list the user saw. Never 0, which means "not sent".
pub fn list_id(paths: &[String]) -> u64 {
    paths.iter().flat_map(|p| p.bytes().chain([0])).fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)).max(1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteDecision {
    Apply,
    Hold,
    NeedsConfirm,
}

/// Spec 6.5: may the destination deletes for one pair run now?
#[allow(clippy::too_many_arguments)]
pub fn decide(
    diff_complete: bool,
    source_files: u64,
    allow_empty: bool,
    destination_files: u64,
    pending: usize,
    outstanding: u64,
    in_cargo: usize,
    confirmed: bool,
    pct: u8,
    count: u32,
) -> DeleteDecision {
    if !diff_complete || (source_files == 0 && !allow_empty) || outstanding > 0 || in_cargo > 0 {
        return DeleteDecision::Hold;
    }
    let pending = pending as u64;
    let big = pending > u64::from(count) || pending * 100 > destination_files.saturating_mul(u64::from(pct));
    if big && !confirmed {
        DeleteDecision::NeedsConfirm
    } else {
        DeleteDecision::Apply
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_queue_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("deletes.json");
        assert_eq!(DeleteQueue::load(&p).unwrap(), DeleteQueue::default());
        let mut q = DeleteQueue::default();
        q.pairs.insert("repositories".into(), PairDeletes {
            paths: vec!["a.txt".into(), "dir/b.txt".into()],
            outstanding: 3, needs_confirm: true, confirmed: false, destination_files: 217,
        });
        q.save(&p).unwrap();
        assert_eq!(DeleteQueue::load(&p).unwrap(), q);
    }

    #[test]
    fn queue_without_destination_files_loads() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("deletes.json");
        std::fs::write(&p, r#"{"pairs":{"r":{"paths":["a"],"outstanding":0,"needs_confirm":false,"confirmed":false}}}"#).unwrap();
        assert_eq!(DeleteQueue::load(&p).unwrap().pairs["r"].destination_files, 0);
    }

    #[test]
    fn queue_with_the_old_home_files_name_loads() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("deletes.json");
        std::fs::write(&p, r#"{"pairs":{"r":{"paths":["a"],"outstanding":0,"needs_confirm":false,"confirmed":false,"home_files":9}}}"#).unwrap();
        assert_eq!(DeleteQueue::load(&p).unwrap().pairs["r"].destination_files, 9);
    }

    // decide(complete, office_files, allow_empty, home_files, pending, outstanding, in_cargo, confirmed, pct, count)
    #[test] fn decide_holds_when_incomplete() {
        assert_eq!(decide(false, 100, false, 100, 0, 0, 0, false, 10, 50), DeleteDecision::Hold);
    }
    #[test] fn decide_holds_when_source_empty() {
        assert_eq!(decide(true, 0, false, 100, 1, 0, 0, false, 10, 50), DeleteDecision::Hold);
        assert_ne!(decide(true, 0, true, 100, 1, 0, 0, false, 10, 50), DeleteDecision::Hold);
    }
    #[test] fn decide_holds_until_caught_up() {
        assert_eq!(decide(true, 100, false, 100, 1, 1, 0, false, 10, 50), DeleteDecision::Hold);
        assert_eq!(decide(true, 100, false, 100, 1, 0, 1, false, 10, 50), DeleteDecision::Hold);
    }
    #[test] fn decide_needs_confirm_over_thresholds() {
        assert_eq!(decide(true, 1000, false, 1000, 51, 0, 0, false, 10, 50), DeleteDecision::NeedsConfirm);
        assert_eq!(decide(true, 100, false, 100, 11, 0, 0, false, 10, 50), DeleteDecision::NeedsConfirm);
        // exactly at thresholds is fine
        assert_eq!(decide(true, 100, false, 100, 10, 0, 0, false, 10, 50), DeleteDecision::Apply);
    }
    #[test] fn decide_applies_when_confirmed() {
        assert_eq!(decide(true, 1000, false, 1000, 51, 0, 0, true, 10, 50), DeleteDecision::Apply);
    }
    #[test] fn decide_applies_small_caught_up() {
        assert_eq!(decide(true, 217, false, 217, 1, 0, 0, false, 10, 50), DeleteDecision::Apply);
    }
}
