use crate::diff::Candidate;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Load {
    pub picked: Vec<Candidate>,
    /// Larger than the whole cargo cap: can never be carried.
    pub oversize: Vec<Candidate>,
}

/// Pairs in priority order; within a pair newest first, first-fit against `budget`.
pub fn pick_load(by_priority: &[Vec<Candidate>], budget: u64, cap_total: u64) -> Load {
    let mut load = Load::default();
    let mut left = budget;
    for pair in by_priority {
        let mut sorted: Vec<&Candidate> = pair.iter().collect();
        sorted.sort_by_key(|c| std::cmp::Reverse(c.mtime));
        for c in sorted {
            if c.size > cap_total {
                load.oversize.push(c.clone());
            } else if c.size <= left {
                left -= c.size;
                load.picked.push(c.clone());
            }
        }
    }
    load
}

/// Bytes we may still add: room under the cap, and room above the free-space floor.
pub fn budget(cap_bytes: u64, carried: u64, disk_free: u64, min_free: u64) -> u64 {
    cap_bytes.saturating_sub(carried).min(disk_free.saturating_sub(min_free))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GB;

    fn c(pair: &str, rel: &str, size: u64, mtime: i64) -> Candidate {
        Candidate { pair: pair.into(), rel: rel.into(), size, mtime }
    }
    fn names(v: &[Candidate]) -> Vec<&str> { v.iter().map(|c| c.rel.as_str()).collect() }

    #[test]
    fn picks_priority_then_newest_first_fit() {
        let p1 = vec![c("p1", "a", 10, 1), c("p1", "b", 30, 3)];
        let p2 = vec![c("p2", "c", 5, 9)];
        assert_eq!(names(&pick_load(&[p1, p2], 38, 100).picked), ["b", "c"]);
    }

    #[test]
    fn oversize_is_flagged_not_picked() {
        let l = pick_load(&[vec![c("p", "big", 60, 1)]], 1000, 50);
        assert_eq!(names(&l.oversize), ["big"]);
        assert!(l.picked.is_empty());
    }

    #[test]
    fn oversize_does_not_consume_budget() {
        let l = pick_load(&[vec![c("p", "big", 60, 9), c("p", "ok", 10, 1)]], 10, 50);
        assert_eq!(names(&l.picked), ["ok"]);
        assert_eq!(names(&l.oversize), ["big"]);
    }

    #[test]
    fn budget_respects_free_space_floor() {
        assert_eq!(budget(50 * GB, 10 * GB, 15 * GB, 10 * GB), 5 * GB);
        assert_eq!(budget(50 * GB, 10 * GB, 5 * GB, 10 * GB), 0);
        assert_eq!(budget(50 * GB, 50 * GB, 500 * GB, 10 * GB), 0);
        assert_eq!(budget(50 * GB, 60 * GB, 500 * GB, 10 * GB), 0);
        assert_eq!(budget(50 * GB, 10 * GB, 500 * GB, 10 * GB), 40 * GB);
    }
}
