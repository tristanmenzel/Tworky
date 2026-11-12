//! Spots Claude Code's working spinner in the terminal titles (OSC 0 and OSC 2) a session writes.

/// The glyph Claude Code shows at the start of its title while it isn't working.
const IDLE_GLYPH: char = '✳';
/// As in xterm.js, so one unterminated sequence can't swallow every later title.
const MAX_OSC: usize = 4096;

const BEL: u8 = 0x07;
const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
}

/// Parses the byte stream across reads. Spinner glyphs are single code points, so the leading
/// `char` stands in for the leading grapheme: a variation selector after it doesn't change it.
#[derive(Default)]
pub struct TitleTracker {
    state: State,
    osc: Vec<u8>,
    glyph: Option<char>,
}

impl TitleTracker {
    /// Whether the chunk held a spinner tick: a title whose leading glyph differs from the
    /// previous title's and isn't the idle glyph.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut ticked = false;
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == ESC {
                        self.state = State::Escape;
                    }
                }
                State::Escape => {
                    self.state = match b {
                        b']' => {
                            self.osc.clear();
                            State::Osc
                        }
                        ESC => State::Escape,
                        _ => State::Ground,
                    }
                }
                State::Osc => match b {
                    BEL => ticked |= self.finish(),
                    ESC => self.state = State::OscEscape,
                    CAN | SUB => self.state = State::Ground,
                    _ if self.osc.len() >= MAX_OSC => self.state = State::Ground,
                    _ => self.osc.push(b),
                },
                State::OscEscape => {
                    if b == b'\\' {
                        ticked |= self.finish();
                    } else {
                        self.state = State::Escape;
                        ticked |= self.feed(&[b]);
                    }
                }
            }
        }
        ticked
    }

    fn finish(&mut self) -> bool {
        self.state = State::Ground;
        let osc = String::from_utf8_lossy(&self.osc);
        let Some(title) = osc.strip_prefix("0;").or_else(|| osc.strip_prefix("2;")) else {
            return false;
        };
        let glyph = title.chars().next();
        let ticked = glyph.is_some_and(|g| g != IDLE_GLYPH) && glyph != self.glyph;
        self.glyph = glyph;
        ticked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn title(text: &str) -> String {
        format!("\x1b]0;{text}\x07")
    }

    fn ticks(tracker: &mut TitleTracker, chunks: &[&[u8]]) -> Vec<bool> {
        chunks.iter().map(|c| tracker.feed(c)).collect()
    }

    #[test]
    fn a_changing_spinner_ticks() {
        let mut t = TitleTracker::default();
        let out = ticks(
            &mut t,
            &[
                title("✢ Fixing bug").as_bytes(),
                title("✳ Fixing bug").as_bytes(),
                title("✶ Fixing bug").as_bytes(),
                title("✻ Fixing bug").as_bytes(),
            ],
        );
        assert_eq!(out, [true, false, true, true]);
    }

    #[test]
    fn the_idle_glyph_never_ticks() {
        let mut t = TitleTracker::default();
        assert!(!t.feed(title("✳ Claude Code").as_bytes()));
        assert!(!t.feed(title("✳ Claude Code").as_bytes()));
    }

    #[test]
    fn a_repeated_glyph_ticks_once() {
        let mut t = TitleTracker::default();
        let out = ticks(
            &mut t,
            &[
                title("· Working").as_bytes(),
                title("· Working").as_bytes(),
                title("· Still working").as_bytes(),
            ],
        );
        assert_eq!(out, [true, false, false]);
    }

    #[test]
    fn osc_2_and_st_terminators_count() {
        let mut t = TitleTracker::default();
        assert!(t.feed("\x1b]2;✢ a\x1b\\".as_bytes()));
        assert!(t.feed("\x1b]0;✶ a\x1b\\".as_bytes()));
        assert!(t.feed("\x1b]2;✻ a\x07".as_bytes()));
    }

    #[test]
    fn a_title_split_across_reads_including_mid_glyph() {
        let mut t = TitleTracker::default();
        let bytes = "text\x1b]0;✢ Working\x1b\\more".as_bytes();
        let glyph_at = bytes.iter().position(|&b| b == b';').unwrap() + 1;
        let cuts = [
            5,
            glyph_at + 1,
            glyph_at + 2,
            bytes.len() - 5,
            bytes.len() - 4,
        ];
        let mut start = 0;
        let mut out = Vec::new();
        for cut in cuts {
            out.push(t.feed(&bytes[start..cut]));
            start = cut;
        }
        out.push(t.feed(&bytes[start..]));
        assert_eq!(out.iter().filter(|&&x| x).count(), 1);
        assert_eq!(t.glyph, Some('✢'));
    }

    #[test]
    fn other_oscs_are_ignored() {
        let mut t = TitleTracker::default();
        assert!(t.feed(title("✢ a").as_bytes()));
        for other in [
            "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
            "\x1b]133;A\x07",
            "\x1b]7;file://host/c/work\x07",
            "\x1b]1;✶ icon\x07",
        ] {
            assert!(!t.feed(other.as_bytes()), "{other:?}");
        }
        assert_eq!(t.glyph, Some('✢'));
    }

    #[test]
    fn esc_without_backslash_aborts_and_starts_a_new_sequence() {
        let mut t = TitleTracker::default();
        assert!(!t.feed("\x1b]0;✢ never ends\x1b[0m".as_bytes()));
        assert!(t.feed(title("✶ next").as_bytes()));
        assert!(t.feed("\x1b]0;✢ cut\x1b\x1b]0;✻ new\x07".as_bytes()));
        assert_eq!(t.glyph, Some('✻'));
    }

    #[test]
    fn can_and_sub_abort() {
        for abort in ["\x18", "\x1a"] {
            let mut t = TitleTracker::default();
            assert!(!t.feed(format!("\x1b]0;✢ a{abort}\x07").as_bytes()));
            assert!(t.feed(title("✶ b").as_bytes()));
        }
    }

    #[test]
    fn an_overlong_osc_is_abandoned() {
        let mut t = TitleTracker::default();
        let long = format!("\x1b]0;✢ {}", "x".repeat(MAX_OSC));
        assert!(!t.feed(long.as_bytes()));
        assert!(!t.feed(b"\x07"));
        assert!(t.feed(title("✶ next").as_bytes()));
    }
}
