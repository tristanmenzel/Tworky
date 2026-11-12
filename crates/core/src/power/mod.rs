use serde::Serialize;

use crate::settings::KeepAwake;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod other;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
use other as os;
#[cfg(windows)]
use windows as os;

pub use os::{SleepLock, has_battery, power_source};

/// What `powercfg /requests`, `pmset -g assertions` and `systemd-inhibit --list` show.
pub const SLEEP_LOCK_REASON: &str = "Tworky: a Claude session is working";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockKind {
    Windows,
    #[serde(rename = "macos")]
    MacOs,
    Portal,
    Logind,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSource {
    Ac,
    Battery,
    Unknown,
}

/// An unknown source counts as plugged in, so a desktop or a failed read still keeps awake.
pub fn should_hold(mode: KeepAwake, busy_sessions: usize, source: PowerSource) -> bool {
    match mode {
        KeepAwake::Off => false,
        KeepAwake::PluggedIn => busy_sessions > 0 && source != PowerSource::Battery,
        KeepAwake::Always => busy_sessions > 0,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use KeepAwake::*;
    use PowerSource::*;

    #[test]
    fn off_never_holds() {
        for source in [Ac, Battery, Unknown] {
            assert!(!should_hold(Off, 0, source));
            assert!(!should_hold(Off, 3, source));
        }
    }

    #[test]
    fn plugged_in_holds_while_busy_unless_on_battery() {
        assert!(should_hold(PluggedIn, 1, Ac));
        assert!(should_hold(PluggedIn, 2, Unknown));
        assert!(!should_hold(PluggedIn, 1, Battery));
        for source in [Ac, Battery, Unknown] {
            assert!(!should_hold(PluggedIn, 0, source));
        }
    }

    #[test]
    fn lock_kind_serializes_for_the_ui() {
        assert_eq!(
            serde_json::to_string(&LockKind::MacOs).unwrap(),
            "\"macos\""
        );
        assert_eq!(
            serde_json::to_string(&LockKind::Logind).unwrap(),
            "\"logind\""
        );
    }

    #[test]
    fn lock_is_taken_released_and_taken_again() {
        let started = Instant::now();
        for _ in 0..2 {
            match SleepLock::acquire(SLEEP_LOCK_REASON) {
                Ok(mut lock) => {
                    let expected: &[LockKind] = if cfg!(windows) {
                        &[LockKind::Windows]
                    } else if cfg!(target_os = "macos") {
                        &[LockKind::MacOs]
                    } else if cfg!(target_os = "linux") {
                        &[LockKind::Portal, LockKind::Logind]
                    } else {
                        &[LockKind::None]
                    };
                    assert!(expected.contains(&lock.kind()), "{:?}", lock.kind());
                    lock.refresh().unwrap();
                }
                // CI's Linux runner may have neither a portal nor a logind session to inhibit.
                Err(_) if cfg!(target_os = "linux") => {}
                Err(e) => panic!("{e:#}"),
            }
        }
        power_source();
        has_battery();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn always_holds_while_busy_on_any_source() {
        for source in [Ac, Battery, Unknown] {
            assert!(should_hold(Always, 1, source));
            assert!(!should_hold(Always, 0, source));
        }
    }
}
