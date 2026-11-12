//! Keeps the host from idle-sleeping while a session's Claude is working, judged by its title
//! spinner (`title.rs`).

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tworky_core::activity;
use tworky_core::config::Paths;
use tworky_core::power::{self, LockKind, PowerSource, SLEEP_LOCK_REASON, SleepLock};
use tworky_core::settings::{KeepAwake, Settings};

use crate::{AppState, CmdResult, err};

/// A session is busy this long after its last spinner tick, and a longer gap starts a new run.
pub const BUSY_WINDOW: Duration = Duration::from_secs(5);
const STUCK_AFTER: Duration = Duration::from_secs(15 * 60);
const POLL: Duration = Duration::from_secs(1);
const POWER_POLL: Duration = Duration::from_secs(15);
const LOCK_RETRY: Duration = Duration::from_secs(60);

/// A session's current run of spinner ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticks {
    pub run_start: Instant,
    pub last: Instant,
}

impl Ticks {
    pub fn after(previous: Option<Ticks>, now: Instant) -> Ticks {
        match previous {
            Some(t) if now.saturating_duration_since(t.last) <= BUSY_WINDOW => {
                Ticks { last: now, ..t }
            }
            _ => Ticks {
                run_start: now,
                last: now,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeepAwakeStatus {
    pub mode: KeepAwake,
    pub holding: bool,
    pub busy_sessions: usize,
    pub on_battery: Option<bool>,
    pub has_battery: bool,
    pub lock: LockKind,
}

impl Default for KeepAwakeStatus {
    fn default() -> Self {
        KeepAwakeStatus {
            mode: KeepAwake::default(),
            holding: false,
            busy_sessions: 0,
            on_battery: None,
            has_battery: false,
            lock: LockKind::None,
        }
    }
}

/// The part of `AppState` the commands and the controller share.
#[derive(Default)]
pub struct Control {
    pub mode: KeepAwake,
    pub status: KeepAwakeStatus,
}

/// A run that ticks for `STUCK_AFTER` without a write to the session's activity file is a hung
/// tool or a spinner that never stops. The window starts at the later of the run's start and the
/// last write, both as `Instant`s the controller saw, so a wall-clock step can't stretch it.
fn is_stuck(ticks: Ticks, last_write: Option<Instant>, now: Instant) -> bool {
    let since = last_write.map_or(ticks.run_start, |w| w.max(ticks.run_start));
    now.saturating_duration_since(since) >= STUCK_AFTER
}

/// Counts busy sessions, leaving out each stuck one until its run of ticks ends.
#[derive(Default)]
struct BusyCounter {
    mtimes: Option<HashMap<String, SystemTime>>,
    last_write: HashMap<String, Instant>,
    stuck_runs: HashMap<String, Instant>,
}

impl BusyCounter {
    fn count(
        &mut self,
        ticks: &HashMap<String, Ticks>,
        mtimes: HashMap<String, SystemTime>,
        now: Instant,
    ) -> usize {
        if let Some(before) = &self.mtimes {
            for (id, mtime) in &mtimes {
                if before.get(id) != Some(mtime) {
                    self.last_write.insert(id.clone(), now);
                }
            }
        }
        self.last_write.retain(|id, _| mtimes.contains_key(id));
        self.mtimes = Some(mtimes);
        self.stuck_runs
            .retain(|id, run| ticks.get(id).is_some_and(|t| t.run_start == *run));

        let mut busy = 0;
        for (id, &t) in ticks {
            if now.saturating_duration_since(t.last) >= BUSY_WINDOW
                || self.stuck_runs.contains_key(id)
            {
                continue;
            }
            if is_stuck(t, self.last_write.get(id).copied(), now) {
                eprintln!("keep awake: {id} has ticked for 15 min without activity; ignoring it");
                self.stuck_runs.insert(id.clone(), t.run_start);
                continue;
            }
            busy += 1;
        }
        busy
    }
}

trait Lock {
    fn refresh(&mut self) -> Result<()>;
    fn kind(&self) -> LockKind;
}

impl Lock for SleepLock {
    fn refresh(&mut self) -> Result<()> {
        SleepLock::refresh(self)
    }

    fn kind(&self) -> LockKind {
        SleepLock::kind(self)
    }
}

/// The controller's state between ticks. OS calls come in through `step`'s arguments.
struct Controller<L> {
    has_battery: bool,
    busy: BusyCounter,
    mode: Option<KeepAwake>,
    source: Option<(Instant, PowerSource)>,
    lock: Option<L>,
    last_failure: Option<Instant>,
}

impl<L: Lock> Controller<L> {
    fn new(has_battery: bool) -> Self {
        Controller {
            has_battery,
            busy: BusyCounter::default(),
            mode: None,
            source: None,
            lock: None,
            last_failure: None,
        }
    }

    fn step(
        &mut self,
        mode: KeepAwake,
        ticks: &HashMap<String, Ticks>,
        mtimes: HashMap<String, SystemTime>,
        now: Instant,
        power_source: impl FnOnce() -> PowerSource,
        acquire: impl FnOnce() -> Result<L>,
    ) -> KeepAwakeStatus {
        let busy = self.busy.count(ticks, mtimes, now);
        if self.mode != Some(mode) {
            self.mode = Some(mode);
            self.source = None;
        }
        if mode == KeepAwake::PluggedIn
            && busy > 0
            && self
                .source
                .is_none_or(|(read, _)| now.saturating_duration_since(read) >= POWER_POLL)
        {
            self.source = Some((now, power_source()));
        }
        let source = self.source.map_or(PowerSource::Unknown, |(_, s)| s);

        if power::should_hold(mode, busy, source) {
            if let Some(lock) = &mut self.lock
                && let Err(e) = lock.refresh()
            {
                eprintln!("keep awake: lost the sleep lock: {e:#}");
                self.lock = None;
                self.last_failure = Some(now);
            }
            if self.lock.is_none()
                && self
                    .last_failure
                    .is_none_or(|at| now.saturating_duration_since(at) >= LOCK_RETRY)
            {
                match acquire() {
                    Ok(lock) => {
                        self.lock = Some(lock);
                        self.last_failure = None;
                    }
                    Err(e) => {
                        eprintln!("keep awake: could not take the sleep lock: {e:#}");
                        self.last_failure = Some(now);
                    }
                }
            }
        } else {
            self.lock = None;
        }

        KeepAwakeStatus {
            mode,
            holding: self.lock.is_some(),
            busy_sessions: busy,
            on_battery: match self.source {
                Some((_, PowerSource::Battery)) => Some(true),
                Some((_, PowerSource::Ac)) => Some(false),
                _ => None,
            },
            has_battery: self.has_battery,
            lock: self.lock.as_ref().map_or(LockKind::None, |l| l.kind()),
        }
    }
}

/// Runs for the life of the app, ticking every second or as soon as the mode changes.
pub fn run(app: AppHandle) {
    let Ok(paths) = Paths::resolve() else { return };
    let state = app.state::<AppState>();
    let mut controller = Controller::<SleepLock>::new(power::has_battery());
    let mut emitted = None;
    {
        let mut control = state.keep_awake.lock().unwrap();
        control.mode = Settings::load(&paths).keep_awake;
    }
    loop {
        let mode = state.keep_awake.lock().unwrap().mode;
        let status = controller.step(
            mode,
            &state.sessions.ticks(),
            activity::modified_all(&paths),
            Instant::now(),
            power::power_source,
            || SleepLock::acquire(SLEEP_LOCK_REASON),
        );
        if emitted.as_ref() != Some(&status) {
            let was_holding = emitted
                .as_ref()
                .is_some_and(|e: &KeepAwakeStatus| e.holding);
            if was_holding != status.holding {
                let verb = if status.holding {
                    "holding"
                } else {
                    "released"
                };
                eprintln!("keep awake: {verb} ({} busy)", status.busy_sessions);
            }
            let _ = app.emit("keep-awake-changed", &status);
            emitted = Some(status.clone());
        }
        let mut control = state.keep_awake.lock().unwrap();
        control.status = status;
        let _ = state
            .keep_awake_changed
            .wait_timeout_while(control, POLL, |c| c.mode == mode);
    }
}

#[tauri::command]
pub fn keep_awake_status(state: State<'_, AppState>) -> KeepAwakeStatus {
    state.keep_awake.lock().unwrap().status.clone()
}

#[tauri::command]
pub fn set_keep_awake(state: State<'_, AppState>, mode: KeepAwake) -> CmdResult<()> {
    let paths = Paths::resolve().map_err(err)?;
    let mut control = state.keep_awake.lock().unwrap();
    Settings { keep_awake: mode }.save(&paths).map_err(err)?;
    control.mode = mode;
    state.keep_awake_changed.notify_all();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);
    const SEC: Duration = Duration::from_secs(1);

    struct Session {
        ticks: Option<Ticks>,
        mtime: SystemTime,
    }

    /// Drives a `BusyCounter` for one session a second at a time.
    struct Sim {
        counter: BusyCounter,
        now: Instant,
        session: Session,
    }

    impl Sim {
        fn new() -> Self {
            Sim {
                counter: BusyCounter::default(),
                now: Instant::now(),
                session: Session {
                    ticks: None,
                    mtime: SystemTime::UNIX_EPOCH,
                },
            }
        }

        /// Advances `secs` seconds, ticking each second when `ticking`, writing the activity file
        /// every `write_every` seconds, and returns the last busy count.
        fn run(&mut self, secs: u64, ticking: bool, write_every: Option<u64>) -> usize {
            let mut busy = 0;
            for i in 1..=secs {
                self.now += SEC;
                if ticking {
                    self.session.ticks = Some(Ticks::after(self.session.ticks, self.now));
                }
                if write_every.is_some_and(|n| i % n == 0) {
                    self.write();
                }
                busy = self.count();
            }
            busy
        }

        fn write(&mut self) {
            self.session.mtime += SEC;
        }

        fn count(&mut self) -> usize {
            let ticks = self
                .session
                .ticks
                .map(|t| HashMap::from([("acme-1".to_string(), t)]))
                .unwrap_or_default();
            let mtimes = HashMap::from([("acme-1".to_string(), self.session.mtime)]);
            self.counter.count(&ticks, mtimes, self.now)
        }
    }

    #[test]
    fn ticks_continue_a_run_within_the_busy_window_and_start_one_after() {
        let start = Instant::now();
        let t = Ticks::after(None, start);
        let t = Ticks::after(Some(t), start + 5 * SEC);
        assert_eq!(t.run_start, start);
        let t = Ticks::after(Some(t), start + 11 * SEC);
        assert_eq!(t.run_start, start + 11 * SEC);
    }

    #[test]
    fn busy_only_while_ticks_are_recent() {
        let mut sim = Sim::new();
        sim.count();
        assert_eq!(sim.run(3, true, None), 1);
        assert_eq!(sim.run(4, false, None), 1);
        assert_eq!(sim.run(1, false, None), 0);
    }

    #[test]
    fn a_run_without_writes_is_stuck_until_its_ticks_stop() {
        let mut sim = Sim::new();
        sim.count();
        assert_eq!(sim.run(15 * 60, true, None), 1);
        assert_eq!(sim.run(1, true, None), 0);
        sim.write();
        assert_eq!(sim.run(10 * 60, true, None), 0, "a write doesn't end it");

        sim.run(6, false, None);
        assert_eq!(sim.run(1, true, None), 1, "a new run is judged afresh");
        assert_eq!(sim.run(15 * 60 - 1, true, None), 1);
        assert_eq!(sim.run(1, true, None), 0, "a second episode");
    }

    #[test]
    fn a_short_pause_does_not_end_a_stuck_run() {
        let mut sim = Sim::new();
        sim.count();
        assert_eq!(sim.run(15 * 60 + 1, true, None), 0);
        sim.run(4, false, None);
        assert_eq!(sim.run(1, true, None), 0);
    }

    #[test]
    fn the_window_starts_at_the_run_when_the_last_write_is_older() {
        let mut sim = Sim::new();
        sim.count();
        sim.write();
        sim.run(10 * 60, false, None);
        assert_eq!(sim.run(15 * 60, true, None), 1);
        assert_eq!(sim.run(1, true, None), 0);
    }

    #[test]
    fn the_window_starts_at_the_last_write_within_the_run() {
        let mut sim = Sim::new();
        sim.count();
        sim.run(10 * 60, true, None);
        sim.write();
        assert_eq!(sim.run(15 * 60, true, None), 1);
        assert_eq!(sim.run(1, true, None), 0);
    }

    #[test]
    fn regular_writes_keep_a_ticking_session_busy() {
        let mut sim = Sim::new();
        sim.count();
        assert_eq!(sim.run(2 * 60 * 60, true, Some(5 * 60)), 1);
    }

    #[test]
    fn writes_seen_before_the_first_poll_are_not_counted() {
        let mut counter = BusyCounter::default();
        let now = Instant::now();
        let mtimes = HashMap::from([("acme-1".to_string(), SystemTime::now())]);
        counter.count(&HashMap::new(), mtimes, now);
        assert!(counter.last_write.is_empty());
    }

    #[derive(Debug)]
    struct FakeLock {
        fail_refresh: bool,
    }

    impl Lock for FakeLock {
        fn refresh(&mut self) -> Result<()> {
            if self.fail_refresh {
                anyhow::bail!("gone");
            }
            Ok(())
        }

        fn kind(&self) -> LockKind {
            LockKind::Logind
        }
    }

    fn busy(now: Instant) -> HashMap<String, Ticks> {
        HashMap::from([(
            "acme-1".to_string(),
            Ticks {
                run_start: now,
                last: now,
            },
        )])
    }

    fn step(
        c: &mut Controller<FakeLock>,
        mode: KeepAwake,
        ticks: &HashMap<String, Ticks>,
        now: Instant,
        source: PowerSource,
        acquire: Result<FakeLock>,
    ) -> (KeepAwakeStatus, bool, bool) {
        let (mut read, mut tried) = (false, false);
        let status = c.step(
            mode,
            ticks,
            HashMap::new(),
            now,
            || {
                read = true;
                source
            },
            || {
                tried = true;
                acquire
            },
        );
        (status, read, tried)
    }

    fn ok() -> Result<FakeLock> {
        Ok(FakeLock {
            fail_refresh: false,
        })
    }

    #[test]
    fn holds_while_busy_and_releases_when_idle() {
        let mut c = Controller::new(false);
        let now = Instant::now();
        let (status, read, _) = step(
            &mut c,
            KeepAwake::Always,
            &busy(now),
            now,
            PowerSource::Ac,
            ok(),
        );
        assert!(!read, "power isn't read in always mode");
        assert_eq!(
            status,
            KeepAwakeStatus {
                mode: KeepAwake::Always,
                holding: true,
                busy_sessions: 1,
                on_battery: None,
                has_battery: false,
                lock: LockKind::Logind,
            }
        );
        let (status, _, _) = step(
            &mut c,
            KeepAwake::Always,
            &HashMap::new(),
            now + SEC,
            PowerSource::Ac,
            ok(),
        );
        assert!(!status.holding);
        assert_eq!(status.lock, LockKind::None);
    }

    #[test]
    fn plugged_in_reads_power_at_most_every_15_seconds_and_only_while_busy() {
        let mut c = Controller::new(true);
        let now = Instant::now();
        let (_, read, _) = step(
            &mut c,
            KeepAwake::PluggedIn,
            &HashMap::new(),
            now,
            PowerSource::Ac,
            ok(),
        );
        assert!(!read);
        let (status, read, _) = step(
            &mut c,
            KeepAwake::PluggedIn,
            &busy(now),
            now,
            PowerSource::Battery,
            ok(),
        );
        assert!(read);
        assert_eq!((status.holding, status.on_battery), (false, Some(true)));
        let later = now + 14 * SEC;
        let (_, read, _) = step(
            &mut c,
            KeepAwake::PluggedIn,
            &busy(later),
            later,
            PowerSource::Ac,
            ok(),
        );
        assert!(!read);
        let later = now + 15 * SEC;
        let (status, read, _) = step(
            &mut c,
            KeepAwake::PluggedIn,
            &busy(later),
            later,
            PowerSource::Ac,
            ok(),
        );
        assert!(read);
        assert_eq!((status.holding, status.on_battery), (true, Some(false)));

        let (status, _, _) = step(
            &mut c,
            KeepAwake::Always,
            &busy(later),
            later,
            PowerSource::Ac,
            ok(),
        );
        assert_eq!(status.on_battery, None, "not read in this mode");
    }

    #[test]
    fn off_never_takes_the_lock() {
        let mut c = Controller::new(false);
        let now = Instant::now();
        let (status, read, tried) = step(
            &mut c,
            KeepAwake::Off,
            &busy(now),
            now,
            PowerSource::Ac,
            ok(),
        );
        assert!(!read && !tried && !status.holding);
        assert_eq!(status.busy_sessions, 1);
    }

    #[test]
    fn a_failed_lock_is_retried_once_a_minute() {
        let mut c = Controller::new(false);
        let now = Instant::now();
        let (status, _, tried) = step(
            &mut c,
            KeepAwake::Always,
            &busy(now),
            now,
            PowerSource::Ac,
            Err(anyhow::anyhow!("no")),
        );
        assert!(tried && !status.holding);
        let later = now + MIN - SEC;
        let (_, _, tried) = step(
            &mut c,
            KeepAwake::Always,
            &busy(later),
            later,
            PowerSource::Ac,
            ok(),
        );
        assert!(!tried);
        let later = now + MIN;
        let (status, _, tried) = step(
            &mut c,
            KeepAwake::Always,
            &busy(later),
            later,
            PowerSource::Ac,
            ok(),
        );
        assert!(tried && status.holding);
    }

    #[test]
    fn a_lost_lock_is_dropped_and_retried_a_minute_later() {
        let mut c = Controller::new(false);
        let now = Instant::now();
        let failing = Ok(FakeLock { fail_refresh: true });
        let (status, _, _) = step(
            &mut c,
            KeepAwake::Always,
            &busy(now),
            now,
            PowerSource::Ac,
            failing,
        );
        assert!(status.holding);
        let later = now + SEC;
        let (status, _, tried) = step(
            &mut c,
            KeepAwake::Always,
            &busy(later),
            later,
            PowerSource::Ac,
            ok(),
        );
        assert!(!tried && !status.holding);
        let later = now + SEC + MIN;
        let (status, _, tried) = step(
            &mut c,
            KeepAwake::Always,
            &busy(later),
            later,
            PowerSource::Ac,
            ok(),
        );
        assert!(tried && status.holding);
    }
}
