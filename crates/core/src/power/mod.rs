use crate::settings::KeepAwake;

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
    fn always_holds_while_busy_on_any_source() {
        for source in [Ac, Battery, Unknown] {
            assert!(should_hold(Always, 1, source));
            assert!(!should_hold(Always, 0, source));
        }
    }
}
