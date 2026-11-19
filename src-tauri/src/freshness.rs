//! Which sessions' hooks still write their activity records. Each window is timed from the
//! `Instant` a poll saw the record's beat change, so the app's own writes and wall-clock steps can't
//! stretch or cut it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A record no hook has written for this long belongs to a hung or vanished session.
pub const STALE_AFTER: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub struct Freshness {
    seen: HashMap<String, (u64, Instant)>,
}

impl Freshness {
    /// A record missing from a poll keeps its time, so a poll that misses files can't pass for a
    /// write when they reappear.
    pub fn poll<'a>(&mut self, beats: impl IntoIterator<Item = (&'a str, u64)>, now: Instant) {
        for (id, beat) in beats {
            match self.seen.get_mut(id) {
                Some((seen, _)) if *seen == beat => {}
                Some(entry) => *entry = (beat, now),
                None => {
                    self.seen.insert(id.to_string(), (beat, now));
                }
            }
        }
    }

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

    #[test]
    fn a_new_beat_starts_the_window() {
        let mut f = Freshness::default();
        let start = Instant::now();
        f.poll([("acme-1", 0)], start);
        let later = start + 20 * MIN;
        f.poll([("acme-1", 1)], later);
        assert!(!f.is_stale("acme-1", later + STALE_AFTER - SEC));
        assert!(f.is_stale("acme-1", later + STALE_AFTER));
    }

    #[test]
    fn an_unchanged_beat_does_not() {
        let mut f = Freshness::default();
        let start = Instant::now();
        f.poll([("acme-1", 3)], start);
        f.poll([("acme-1", 3)], start + 20 * MIN);
        assert!(f.is_stale("acme-1", start + STALE_AFTER));
    }

    #[test]
    fn a_first_sighting_starts_the_window_at_the_first_poll_and_later() {
        let mut f = Freshness::default();
        let start = Instant::now();
        f.poll([("acme-1", 0)], start);
        assert!(!f.is_stale("acme-1", start));
        assert!(f.is_stale("acme-2", start), "never seen");

        let later = start + 10 * MIN;
        f.poll([("acme-1", 0), ("acme-2", 0)], later);
        assert!(f.is_stale("acme-1", start + STALE_AFTER));
        assert!(!f.is_stale("acme-2", start + STALE_AFTER));
        assert!(f.is_stale("acme-2", later + STALE_AFTER));
    }

    #[test]
    fn a_missing_record_in_one_poll_is_not_a_change() {
        let mut f = Freshness::default();
        let start = Instant::now();
        f.poll([("acme-1", 0)], start);
        f.poll([("acme-2", 0)], start + 10 * MIN);
        f.poll([], start + 15 * MIN);
        f.poll([("acme-1", 0)], start + 20 * MIN);
        assert!(f.is_stale("acme-1", start + STALE_AFTER));
    }

    #[test]
    fn stale_after_30_minutes_without_a_beat() {
        let mut f = Freshness::default();
        let start = Instant::now();
        f.poll([("acme-1", 0)], start);
        let mut now = start;
        while now < start + 30 * MIN - SEC {
            now += SEC;
            f.poll([("acme-1", 0)], now);
        }
        assert!(!f.is_stale("acme-1", now));
        now += SEC;
        assert!(f.is_stale("acme-1", now));
    }
}
