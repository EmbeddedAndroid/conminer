//! Boot-mode overrides a board controller holds across boots.
//!
//! A strap-latching controller (the Bantam) keeps a boot-mode line asserted
//! until something releases it. That is the design: a flash needs the board to
//! come back into EDL after every reset. It is also a trap, because the line
//! survives power cycles and nothing on the board shows it: a board held this
//! way sits in ROM EDL with a silent console, which looks like dead firmware.
//! conminer could set that line and clear it; it could not show it.
//!
//! Two different facts, never merged.
//!
//! * What the CONTROLLER holds is intent for the next boot. It is read from the
//!   controller, by a hook that only ever queries.
//! * What USB shows (a QDL gadget on the board's ports) is an observation of
//!   what the board is doing now.
//!
//! Either can be true without the other: a strap set a second ago has not put
//! the running board anywhere yet, and a board can enter EDL on its own with no
//! strap at all. So this module knows nothing about USB, and the two are
//! reported side by side.
//!
//! And a read that failed is `unknown`, not "released". Every consumer that
//! matters (the dashboard chip, the normal-boot gate) is asking "is it safe to
//! assume this board boots normally", and a controller that did not answer has
//! not said yes.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// One override line, as the controller reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Read back as 0: not held.
    Released,
    /// Read back as 1: held, and it will still be held after a power cycle.
    Asserted,
    /// The controller did not give a clean 0 or 1.
    Unknown,
}

impl Level {
    fn parse(value: &str) -> Self {
        match value.trim() {
            "0" => Level::Released,
            "1" => Level::Asserted,
            _ => Level::Unknown,
        }
    }

    /// `0`, `1` or `null`. Never a guess.
    pub fn to_json(self) -> Value {
        match self {
            Level::Released => json!(0),
            Level::Asserted => json!(1),
            Level::Unknown => Value::Null,
        }
    }
}

/// What a whole read amounts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Summary {
    /// Every override was read, and every one is released.
    Clear,
    /// At least one override is asserted. Positive evidence, so it wins over an
    /// unreadable neighbour: one line known to be held is enough to say the next
    /// boot is not a normal one.
    Latched,
    /// Nothing is known to be held, but not everything could be read.
    Unknown,
}

impl Summary {
    pub fn as_str(self) -> &'static str {
        match self {
            Summary::Clear => "clear",
            Summary::Latched => "latched",
            Summary::Unknown => "unknown",
        }
    }
}

/// One read of a controller's boot-mode overrides.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BootOverrides {
    /// In the order the controller reported them.
    pub signals: Vec<(String, Level)>,
    /// `(mode, line)`: which boot mode holds which line, as the HOOK declared it.
    ///
    /// From the hook and nowhere else, because the hook is what turns a mode
    /// into a line when it sets one. A second copy of that table in a config
    /// file is a copy that can light the wrong button.
    pub modes: Vec<(String, String)>,
}

impl BootOverrides {
    /// Parse a read hook's stdout: one `NAME=0|1|<anything else>` per line.
    ///
    /// Lines that are not `NAME=value` are ignored, so a hook may print a status
    /// trailer. A NAME is upper-case letters, digits and underscores, which is
    /// what a signal is called on every controller this has met and is narrow
    /// enough that prose is never mistaken for one.
    ///
    /// A hook may also say which boot mode holds which line, one
    /// `mode <MODE> asserts <LINE>` per mode. Optional: without it the lines are
    /// still reported, and no mode can be shown as held.
    pub fn parse(stdout: &str) -> Self {
        let is_name = |name: &str| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        };
        let mut signals: Vec<(String, Level)> = Vec::new();
        let mut modes: Vec<(String, String)> = Vec::new();
        for line in stdout.lines() {
            let words: Vec<&str> = line.split_whitespace().collect();
            if let ["mode", mode, "asserts", held] = words[..] {
                if is_name(mode) && is_name(held) {
                    let pair = (mode.to_string(), held.to_string());
                    if !modes.contains(&pair) {
                        modes.push(pair);
                    }
                }
                continue;
            }
            let Some((name, value)) = line.trim().split_once('=') else {
                continue;
            };
            let name = name.trim();
            if !is_name(name) {
                continue;
            }
            let level = Level::parse(value);
            // A name reported twice with different answers is not knowledge.
            match signals.iter_mut().find(|(n, _)| n == name) {
                Some((_, prev)) if *prev != level => *prev = Level::Unknown,
                Some(_) => {}
                None => signals.push((name.to_string(), level)),
            }
        }
        Self { signals, modes }
    }

    /// The read produced nothing at all: the hook failed, timed out, or printed
    /// no signal lines.
    pub fn unreadable() -> Self {
        Self::default()
    }

    pub fn asserted(&self) -> Vec<&str> {
        self.names(Level::Asserted)
    }

    pub fn unknown(&self) -> Vec<&str> {
        self.names(Level::Unknown)
    }

    fn names(&self, level: Level) -> Vec<&str> {
        self.signals
            .iter()
            .filter(|(_, l)| *l == level)
            .map(|(n, _)| n.as_str())
            .collect()
    }

    pub fn summary(&self) -> Summary {
        if !self.asserted().is_empty() {
            Summary::Latched
        } else if self.signals.is_empty() || !self.unknown().is_empty() {
            Summary::Unknown
        } else {
            Summary::Clear
        }
    }

    /// Is it PROVEN that nothing is held? The gate a normal boot must pass.
    ///
    /// Stricter than "nothing asserted": an empty read and a partly unreadable
    /// one both fail it, because the question is whether the next boot is known
    /// to be a normal one, and "the controller did not say" is not that.
    pub fn all_released(&self) -> bool {
        self.summary() == Summary::Clear
    }

    /// Is this boot mode held? `None` when the hook never said which line the
    /// mode holds: a mode that holds nothing (a firmware sequence), or a hook
    /// that does not declare its table.
    ///
    /// Held and released are both PROVEN, by a clean read of the mode's own
    /// line. Everything else is unknown: a line the hook named and did not
    /// report, a line that did not read cleanly, and a mode the hook mapped to
    /// two different lines.
    pub fn mode_level(&self, mode: &str) -> Option<Level> {
        let mut lines = self.modes.iter().filter(|(m, _)| m == mode).map(|(_, l)| l);
        let line = lines.next()?;
        if lines.next().is_some() {
            return Some(Level::Unknown);
        }
        Some(
            self.signals
                .iter()
                .find(|(n, _)| n == line)
                .map(|(_, level)| *level)
                .unwrap_or(Level::Unknown),
        )
    }

    /// `{"BOOT_MD_EDL": {"line": "MD_EDL", "state": "held"}}`, one entry per
    /// mode the hook declared. `state` is `held`, `released` or `unknown`.
    pub fn modes_json(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (mode, line) in &self.modes {
            let state = match self.mode_level(mode) {
                Some(Level::Asserted) => "held",
                Some(Level::Released) => "released",
                _ => "unknown",
            };
            let ambiguous = self.modes.iter().filter(|(m, _)| m == mode).count() > 1;
            out.insert(
                mode.clone(),
                json!({
                    "line": if ambiguous { Value::Null } else { json!(line) },
                    "state": state,
                }),
            );
        }
        Value::Object(out)
    }

    /// `{"MD_EDL": 1, "SS_EDL": 0, "UEFI": null}`.
    pub fn levels_json(&self) -> Value {
        Value::Object(
            self.signals
                .iter()
                .map(|(n, l)| (n.clone(), l.to_json()))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_read_of_released_lines_is_clear() {
        let o = BootOverrides::parse("MD_EDL=0\nSS_EDL=0\nUEFI=0\nFASTBOOT_MD=0\n");
        assert_eq!(o.summary(), Summary::Clear);
        assert!(o.all_released());
        assert!(o.asserted().is_empty());
    }

    #[test]
    fn one_asserted_line_is_latched_and_named() {
        let o = BootOverrides::parse("MD_EDL=1\nSS_EDL=0\nUEFI=0\nFASTBOOT_MD=0\n");
        assert_eq!(o.summary(), Summary::Latched);
        assert_eq!(o.asserted(), vec!["MD_EDL"]);
        assert!(!o.all_released());
    }

    /// The rule the whole feature rests on.
    #[test]
    fn a_line_that_could_not_be_read_is_unknown_never_released() {
        for garbage in ["", "?", "unknown", "2", "01", "0 1", "ERR", "-"] {
            let o = BootOverrides::parse(&format!("MD_EDL={garbage}\nSS_EDL=0\n"));
            assert_eq!(
                o.signals[0].1,
                Level::Unknown,
                "{garbage:?} is not a clean 0 or 1"
            );
            assert_eq!(o.summary(), Summary::Unknown, "{garbage:?}");
            assert!(
                !o.all_released(),
                "a controller that answered {garbage:?} has not said the line is released"
            );
        }
    }

    #[test]
    fn a_read_that_produced_nothing_is_unknown() {
        for out in ["", "\n\n", "bantam-power: boot-overrides ok\n"] {
            let o = BootOverrides::parse(out);
            assert_eq!(o.summary(), Summary::Unknown, "{out:?}");
            assert!(!o.all_released(), "{out:?}");
        }
        assert_eq!(BootOverrides::unreadable().summary(), Summary::Unknown);
    }

    /// Known-held beats could-not-read: one asserted line is enough to say the
    /// next boot is not a normal one, whatever its neighbour did.
    #[test]
    fn an_asserted_line_wins_over_an_unreadable_neighbour() {
        let o = BootOverrides::parse("MD_EDL=1\nSS_EDL=unknown\n");
        assert_eq!(o.summary(), Summary::Latched);
        assert_eq!(o.unknown(), vec!["SS_EDL"]);
    }

    #[test]
    fn prose_and_status_trailers_are_not_signals() {
        let o = BootOverrides::parse(
            "  reading straps\nMD_EDL=0\nbantam-power: boot-overrides ok\nnote=this is prose\n",
        );
        assert_eq!(o.signals, vec![("MD_EDL".to_string(), Level::Released)]);
    }

    #[test]
    fn a_name_answered_two_ways_is_not_knowledge() {
        let o = BootOverrides::parse("MD_EDL=0\nMD_EDL=1\n");
        assert_eq!(o.signals, vec![("MD_EDL".to_string(), Level::Unknown)]);
        assert!(!o.all_released());
    }

    /// What lights a button. Held and released are each proven by the mode's
    /// own line; nothing else is.
    #[test]
    fn a_mode_is_held_only_when_its_own_line_reads_asserted() {
        let o = BootOverrides::parse(
            "MD_EDL=1\nSS_EDL=0\nUEFI=?\n\
             mode BOOT_MD_EDL asserts MD_EDL\n\
             mode BOOT_SS_EDL asserts SS_EDL\n\
             mode BOOT_UEFI asserts UEFI\n\
             mode MD_FASTBOOT asserts FASTBOOT_MD\n",
        );
        assert_eq!(o.mode_level("BOOT_MD_EDL"), Some(Level::Asserted));
        assert_eq!(o.mode_level("BOOT_SS_EDL"), Some(Level::Released));
        assert_eq!(
            o.mode_level("BOOT_UEFI"),
            Some(Level::Unknown),
            "its line did not read cleanly"
        );
        assert_eq!(
            o.mode_level("MD_FASTBOOT"),
            Some(Level::Unknown),
            "the hook named a line and never reported it: that is not released"
        );
        assert_eq!(
            o.mode_level("SS_MD_FASTBOOT"),
            None,
            "a mode the hook did not map holds nothing to show"
        );
        assert_eq!(
            o.modes_json(),
            json!({
                "BOOT_MD_EDL": {"line": "MD_EDL", "state": "held"},
                "BOOT_SS_EDL": {"line": "SS_EDL", "state": "released"},
                "BOOT_UEFI": {"line": "UEFI", "state": "unknown"},
                "MD_FASTBOOT": {"line": "FASTBOOT_MD", "state": "unknown"},
            })
        );
        // The mode table changes nothing about the lines or the summary.
        assert_eq!(o.asserted(), vec!["MD_EDL"]);
        assert_eq!(o.summary(), Summary::Latched);
    }

    #[test]
    fn a_mode_mapped_to_two_lines_is_not_knowledge() {
        let o = BootOverrides::parse(
            "MD_EDL=0\nSS_EDL=0\nmode BOOT_MD_EDL asserts MD_EDL\nmode BOOT_MD_EDL asserts SS_EDL\n",
        );
        assert_eq!(o.mode_level("BOOT_MD_EDL"), Some(Level::Unknown));
        assert_eq!(
            o.modes_json(),
            json!({"BOOT_MD_EDL": {"line": null, "state": "unknown"}})
        );
    }

    #[test]
    fn a_mode_line_is_never_mistaken_for_a_signal_and_prose_is_never_a_mode() {
        let o = BootOverrides::parse(
            "mode BOOT_MD_EDL asserts MD_EDL\nmode is fine\nmode lower asserts MD_EDL\n\
             mode BOOT_UEFI asserts UEFI and more\nMD_EDL=0\n",
        );
        assert_eq!(o.signals, vec![("MD_EDL".to_string(), Level::Released)]);
        assert_eq!(
            o.modes,
            vec![("BOOT_MD_EDL".to_string(), "MD_EDL".to_string())]
        );
        // A failed read declares nothing, so nothing can be shown as released.
        assert_eq!(BootOverrides::unreadable().mode_level("BOOT_MD_EDL"), None);
    }

    #[test]
    fn levels_render_as_zero_one_or_null() {
        let o = BootOverrides::parse("MD_EDL=1\nSS_EDL=0\nUEFI=?\n");
        assert_eq!(
            o.levels_json(),
            json!({"MD_EDL": 1, "SS_EDL": 0, "UEFI": null})
        );
    }
}
