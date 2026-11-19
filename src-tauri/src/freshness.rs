//! Which activity records the sessions' hooks still write. Each window is timed from the `Instant`
//! a poll saw the record's mtime change, never from the mtime itself, so a wall-clock step can't
//! stretch or cut it.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

/// A record not written for this long belongs to a hung or vanished session.
pub const STALE_AFTER: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub struct Freshness {
    seen: HashMap<String, (SystemTime, Instant)>,
}

impl Freshness {
    /// `mtimes` is `None` when the activity folder couldn't be read. A record missing from a poll
    /// keeps its time, so a poll that misses files can't pass for a write when they reappear.
    pub fn poll(&mut self, mtimes: Option<HashMap<String, SystemTime>>, now: Instant) {
        for (id, mtime) in mtimes.into_iter().flatten() {
            match self.seen.get_mut(&id) {
                Some((seen, _)) if *seen == mtime => {}
                Some(entry) => *entry = (mtime, now),
                None => {
                    self.seen.insert(id, (mtime, now));
                }
            }
        }
    }

    /// A record never seen counts as stale.
    pub fn is_stale(&self, id: &str, now: Instant) -> bool {
        self.seen
            .get(id)
            .is_none_or(|&(_, at)| now.saturating_duration_since(at) >= STALE_AFTER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Duration = Duration::from_secs(1);
    const MIN: Duration = Duration::from_secs(60);

    fn mtimes(entries: &[(&str, SystemTime)]) -> Option<HashMap<String, SystemTime>> {
        Some(entries.iter().map(|&(id, t)| (id.to_string(), t)).collect())
    }

    #[test]
    fn a_change_starts_the_window() {
        let mut f = Freshness::default();
        let start = Instant::now();
        let t = SystemTime::UNIX_EPOCH;
        f.poll(mtimes(&[("acme-1", t)]), start);
        let later = start + 20 * MIN;
        f.poll(mtimes(&[("acme-1", t + SEC)]), later);
        assert!(!f.is_stale("acme-1", later + STALE_AFTER - SEC));
        assert!(f.is_stale("acme-1", later + STALE_AFTER));
    }

    #[test]
    fn an_unchanged_mtime_does_not() {
        let mut f = Freshness::default();
        let start = Instant::now();
        let t = SystemTime::UNIX_EPOCH;
        f.poll(mtimes(&[("acme-1", t)]), start);
        f.poll(mtimes(&[("acme-1", t)]), start + 20 * MIN);
        assert!(f.is_stale("acme-1", start + STALE_AFTER));
    }

    #[test]
    fn a_first_sighting_starts_the_window_at_the_first_poll_and_later() {
        let mut f = Freshness::default();
        let start = Instant::now();
        let t = SystemTime::UNIX_EPOCH;
        f.poll(mtimes(&[("acme-1", t)]), start);
        assert!(!f.is_stale("acme-1", start));
        assert!(f.is_stale("acme-2", start), "never seen");

        let later = start + 10 * MIN;
        f.poll(mtimes(&[("acme-1", t), ("acme-2", t)]), later);
        assert!(f.is_stale("acme-1", start + STALE_AFTER));
        assert!(!f.is_stale("acme-2", start + STALE_AFTER));
        assert!(f.is_stale("acme-2", later + STALE_AFTER));
    }

    #[test]
    fn a_missing_file_in_one_poll_is_not_a_change() {
        let t = SystemTime::UNIX_EPOCH;
        let other = mtimes(&[("acme-2", t)]);
        for missing in [None, mtimes(&[]), other] {
            let mut f = Freshness::default();
            let start = Instant::now();
            f.poll(mtimes(&[("acme-1", t)]), start);
            f.poll(missing, start + 10 * MIN);
            f.poll(mtimes(&[("acme-1", t)]), start + 20 * MIN);
            assert!(f.is_stale("acme-1", start + STALE_AFTER));
        }
    }

    #[test]
    fn stale_after_30_minutes_without_a_write() {
        let mut f = Freshness::default();
        let start = Instant::now();
        let t = SystemTime::UNIX_EPOCH;
        f.poll(mtimes(&[("acme-1", t)]), start);
        let mut now = start;
        while now < start + 30 * MIN - SEC {
            now += SEC;
            f.poll(mtimes(&[("acme-1", t)]), now);
        }
        assert!(!f.is_stale("acme-1", now));
        now += SEC;
        assert!(f.is_stale("acme-1", now));
    }
}
