//! How `watch_activity` shows a session's activity record, and when a change flashes the taskbar.

use tworky_core::activity::{AccountRequest, Activity, Handover, Session};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flash {
    Urgent,
    Gentle,
}

/// Background work shows as working only while the session is live and its hooks still write.
/// Returns whether that turned the session idle, a change that flashes nothing.
pub fn settle(session: &mut Session, live: bool, stale: bool) -> bool {
    let quietly_idle = session.activity == Activity::Working
        && session.recorded != Activity::Working
        && (!live || stale);
    if quietly_idle {
        session.activity = Activity::Idle;
    }
    quietly_idle
}

/// What the UI is sent, which a beat alone doesn't change.
pub fn shown(s: &Session) -> (Activity, Option<&Handover>, &[AccountRequest]) {
    (s.activity, s.handover.as_ref(), &s.account_requests)
}

pub fn flash(was: Option<Activity>, is: Option<Activity>, quietly_idle: bool) -> Option<Flash> {
    match (was, is) {
        (_, Some(Activity::Waiting)) => Some(Flash::Urgent),
        (was, Some(Activity::Account)) if was != Some(Activity::Account) => Some(Flash::Urgent),
        (Some(Activity::Working), Some(Activity::Idle)) if !quietly_idle => Some(Flash::Gentle),
        (Some(Activity::Working | Activity::Idle), Some(Activity::Uat)) => Some(Flash::Gentle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(recorded: Activity, background: bool) -> Session {
        Session {
            activity: if recorded == Activity::Idle && background {
                Activity::Working
            } else {
                recorded
            },
            handover: None,
            account_requests: vec![],
            recorded,
            background,
            beat: 0,
        }
    }

    fn shown(mut s: Session, live: bool, stale: bool) -> (Activity, bool) {
        let quietly_idle = settle(&mut s, live, stale);
        (s.activity, quietly_idle)
    }

    #[test]
    fn background_work_of_a_dead_or_stale_session_goes_idle_quietly() {
        for (live, stale) in [(false, false), (true, true), (false, true)] {
            let was = shown(session(Activity::Idle, true), true, false);
            assert_eq!(was, (Activity::Working, false));
            let is = shown(session(Activity::Idle, true), live, stale);
            assert_eq!(is, (Activity::Idle, true), "live {live}, stale {stale}");
            assert_eq!(flash(Some(was.0), Some(is.0), is.1), None);
        }
    }

    #[test]
    fn recorded_working_is_never_downgraded() {
        for (live, stale) in [(true, false), (false, false), (true, true), (false, true)] {
            for background in [false, true] {
                assert_eq!(
                    shown(session(Activity::Working, background), live, stale),
                    (Activity::Working, false)
                );
            }
        }
    }

    #[test]
    fn a_stop_that_clears_background_work_still_flashes() {
        let was = shown(session(Activity::Idle, true), true, false);
        let is = shown(session(Activity::Idle, false), true, false);
        assert_eq!(is, (Activity::Idle, false));
        assert_eq!(flash(Some(was.0), Some(is.0), is.1), Some(Flash::Gentle));
    }

    #[test]
    fn waiting_is_left_alone() {
        for (live, stale) in [(true, false), (false, true)] {
            for background in [false, true] {
                assert_eq!(
                    shown(session(Activity::Waiting, background), live, stale),
                    (Activity::Waiting, false)
                );
            }
        }
        assert_eq!(
            flash(Some(Activity::Working), Some(Activity::Waiting), false),
            Some(Flash::Urgent)
        );
    }
}
