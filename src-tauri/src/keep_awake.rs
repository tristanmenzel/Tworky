//! Keeps the host from idle-sleeping while a session's Claude is busy, as its activity record says.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tworky_core::activity::{self, Activity, Session};
use tworky_core::config::Paths;
use tworky_core::power::{self, LockKind, PowerSource, SLEEP_LOCK_REASON, SleepLock};
use tworky_core::settings::{KeepAwake, Settings};

use crate::freshness::Freshness;
use crate::{AppState, CmdResult, err};

const POLL: Duration = Duration::from_secs(1);
const POWER_POLL: Duration = Duration::from_secs(15);
const LOCK_RETRY: Duration = Duration::from_secs(60);

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

#[derive(Default)]
pub struct Control {
    pub mode: KeepAwake,
    pub status: KeepAwakeStatus,
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

/// OS calls come in through `step`'s arguments so tests can fake them.
struct Controller<L> {
    has_battery: bool,
    freshness: Freshness,
    mode: Option<KeepAwake>,
    source: Option<(Instant, PowerSource)>,
    lock: Option<L>,
    last_failure: Option<Instant>,
}

impl<L: Lock> Controller<L> {
    fn new(has_battery: bool) -> Self {
        Controller {
            has_battery,
            freshness: Freshness::default(),
            mode: None,
            source: None,
            lock: None,
            last_failure: None,
        }
    }

    /// A live session with a fresh record is busy while it records working or lists background
    /// work, whatever its card shows, so a handover posted while a subagent runs still counts.
    fn count_busy(
        &mut self,
        sessions: &HashMap<String, Session>,
        live: &HashSet<String>,
        mtimes: Option<HashMap<String, SystemTime>>,
        now: Instant,
    ) -> usize {
        self.freshness.poll(mtimes, now);
        sessions
            .iter()
            .filter(|(id, s)| {
                live.contains(*id)
                    && !self.freshness.is_stale(id, now)
                    && (s.recorded == Activity::Working || s.background)
            })
            .count()
    }

    fn step(
        &mut self,
        mode: KeepAwake,
        busy: usize,
        now: Instant,
        power_source: impl FnOnce() -> PowerSource,
        acquire: impl FnOnce() -> Result<L>,
    ) -> KeepAwakeStatus {
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
        let sessions = state.activity.lock().unwrap().clone();
        let now = Instant::now();
        let busy = controller.count_busy(
            &sessions,
            &state.sessions.live_ids(),
            activity::modified_all(&paths).ok(),
            now,
        );
        let status = controller.step(mode, busy, now, power::power_source, || {
            SleepLock::acquire(SLEEP_LOCK_REASON)
        });
        state.keep_awake.lock().unwrap().status = status.clone();
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
            emitted = Some(status);
        }
        let control = state.keep_awake.lock().unwrap();
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
    use tworky_core::activity::Handover;

    const MIN: Duration = Duration::from_secs(60);
    const SEC: Duration = Duration::from_secs(1);

    fn session(recorded: Activity, background: bool) -> Session {
        Session {
            activity: recorded,
            handover: None,
            account_requests: vec![],
            recorded,
            background,
        }
    }

    struct Sim {
        controller: Controller<FakeLock>,
        now: Instant,
        session: Session,
        live: bool,
        mtime: SystemTime,
    }

    impl Sim {
        fn new(session: Session) -> Self {
            let mut sim = Sim {
                controller: Controller::new(false),
                now: Instant::now(),
                session,
                live: true,
                mtime: SystemTime::UNIX_EPOCH,
            };
            sim.poll();
            sim
        }

        fn poll(&mut self) -> usize {
            let sessions = HashMap::from([("acme-1".to_string(), self.session.clone())]);
            let live = if self.live {
                HashSet::from(["acme-1".to_string()])
            } else {
                HashSet::new()
            };
            let mtimes = HashMap::from([("acme-1".to_string(), self.mtime)]);
            let busy = self
                .controller
                .count_busy(&sessions, &live, Some(mtimes), self.now);
            let (status, _, _) = step(
                &mut self.controller,
                KeepAwake::Always,
                busy,
                self.now,
                PowerSource::Ac,
                ok(),
            );
            assert_eq!(status.holding, status.busy_sessions > 0);
            status.busy_sessions
        }

        fn run(&mut self, secs: u64, write_every: Option<u64>) -> usize {
            let mut busy = 0;
            for i in 1..=secs {
                self.now += SEC;
                if write_every.is_some_and(|n| i % n == 0) {
                    self.mtime += SEC;
                }
                busy = self.poll();
            }
            busy
        }
    }

    #[test]
    fn busy_while_working() {
        let mut sim = Sim::new(session(Activity::Working, false));
        assert_eq!(sim.poll(), 1);
        sim.session = session(Activity::Idle, false);
        assert_eq!(sim.run(1, None), 0);
    }

    #[test]
    fn still_busy_after_31_minutes_of_working_with_a_write_every_minute() {
        let mut sim = Sim::new(session(Activity::Working, false));
        assert_eq!(sim.run(31 * 60, Some(60)), 1);
    }

    #[test]
    fn busy_while_idle_with_background_work() {
        let mut sim = Sim::new(session(Activity::Idle, true));
        assert_eq!(sim.poll(), 1);
    }

    #[test]
    fn busy_while_handed_over_with_background_work() {
        let mut uat = session(Activity::Idle, true);
        uat.activity = Activity::Uat;
        uat.handover = Some(Handover {
            summary: "s".into(),
            steps: vec![],
            tabs: vec![],
        });
        let mut sim = Sim::new(uat);
        assert_eq!(sim.poll(), 1);
        sim.session.background = false;
        assert_eq!(sim.run(1, None), 0);
    }

    #[test]
    fn not_busy_when_dead() {
        let mut sim = Sim::new(session(Activity::Working, true));
        sim.live = false;
        assert_eq!(sim.run(1, Some(1)), 0);
    }

    #[test]
    fn not_busy_after_30_minutes_without_a_write() {
        for background in [false, true] {
            let mut sim = Sim::new(session(Activity::Working, background));
            assert_eq!(sim.run(30 * 60 - 1, None), 1);
            assert_eq!(sim.run(1, None), 0);
            assert_eq!(sim.run(1, Some(1)), 1, "a write makes it fresh again");
        }
    }

    #[test]
    fn a_session_whose_record_time_was_never_read_is_not_busy() {
        let mut c = Controller::<FakeLock>::new(false);
        let live = HashSet::from(["acme-1".to_string()]);
        let sessions = HashMap::from([("acme-1".to_string(), session(Activity::Working, false))]);
        assert_eq!(c.count_busy(&sessions, &live, None, Instant::now()), 0);
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

    fn step(
        c: &mut Controller<FakeLock>,
        mode: KeepAwake,
        busy: usize,
        now: Instant,
        source: PowerSource,
        acquire: Result<FakeLock>,
    ) -> (KeepAwakeStatus, bool, bool) {
        let (mut read, mut tried) = (false, false);
        let status = c.step(
            mode,
            busy,
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
        let (status, read, _) = step(&mut c, KeepAwake::Always, 1, now, PowerSource::Ac, ok());
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
            0,
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
        let (_, read, _) = step(&mut c, KeepAwake::PluggedIn, 0, now, PowerSource::Ac, ok());
        assert!(!read);
        let (status, read, _) = step(
            &mut c,
            KeepAwake::PluggedIn,
            1,
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
            1,
            later,
            PowerSource::Ac,
            ok(),
        );
        assert!(!read);
        let later = now + 15 * SEC;
        let (status, read, _) = step(
            &mut c,
            KeepAwake::PluggedIn,
            1,
            later,
            PowerSource::Ac,
            ok(),
        );
        assert!(read);
        assert_eq!((status.holding, status.on_battery), (true, Some(false)));

        let (status, _, _) = step(&mut c, KeepAwake::Always, 1, later, PowerSource::Ac, ok());
        assert_eq!(status.on_battery, None, "not read in this mode");
    }

    #[test]
    fn off_never_takes_the_lock() {
        let mut c = Controller::new(false);
        let now = Instant::now();
        let (status, read, tried) = step(&mut c, KeepAwake::Off, 1, now, PowerSource::Ac, ok());
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
            1,
            now,
            PowerSource::Ac,
            Err(anyhow::anyhow!("no")),
        );
        assert!(tried && !status.holding);
        let later = now + MIN - SEC;
        let (_, _, tried) = step(&mut c, KeepAwake::Always, 1, later, PowerSource::Ac, ok());
        assert!(!tried);
        let later = now + MIN;
        let (status, _, tried) = step(&mut c, KeepAwake::Always, 1, later, PowerSource::Ac, ok());
        assert!(tried && status.holding);
    }

    #[test]
    fn a_lost_lock_is_dropped_and_retried_a_minute_later() {
        let mut c = Controller::new(false);
        let now = Instant::now();
        let failing = Ok(FakeLock { fail_refresh: true });
        let (status, _, _) = step(&mut c, KeepAwake::Always, 1, now, PowerSource::Ac, failing);
        assert!(status.holding);
        let later = now + SEC;
        let (status, _, tried) = step(&mut c, KeepAwake::Always, 1, later, PowerSource::Ac, ok());
        assert!(!tried && !status.holding);
        let later = now + SEC + MIN;
        let (status, _, tried) = step(&mut c, KeepAwake::Always, 1, later, PowerSource::Ac, ok());
        assert!(tried && status.holding);
    }
}
