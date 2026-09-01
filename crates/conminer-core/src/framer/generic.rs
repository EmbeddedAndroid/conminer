//! Cross-cutting generic pattern classes (§A.10).
//!
//! These are project-independent: they form the `raw` profile's heuristics, the
//! fallback for unknown firmware, and the shared building blocks every profile in
//! Appendix A references. Owning them once means a new profile declares *which*
//! classes apply and adds its own strings, rather than restating regexes that are
//! the same everywhere.

use crate::store::Severity;
use once_cell::sync::Lazy;
use regex::Regex;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        pub static $name: Lazy<Regex> = Lazy::new(|| Regex::new($pat).expect(stringify!($name)));
    };
}

// `ERROR`/`WARN(ING)`/`FATAL`/`PANIC`/`BUG`/`FAIL(ED)`/`ASSERT` as word tokens,
// any case, including the single-letter prefix forms (`E:`, `E/`, `E (`).
re!(
    SEVERITY_FATAL,
    r"(?i)\b(fatal|panic|panicked|oops|emerg)\b|^\s*[FE]/[A-Z]{2}:"
);
re!(
    SEVERITY_ERROR,
    r"(?i)\b(error|err|fail(ed|ure)?|assert(ion)?|abort(ed)?|bug|exception)\b|^\s*(E:|E/|E \(|<3>|ERROR:)"
);
re!(
    SEVERITY_WARN,
    r"(?i)\b(warn(ing)?|deprecated)\b|^\s*(W:|W/|W \(|<4>|WARNING:)"
);
re!(SEVERITY_NOTICE, r"^\s*(NOTICE:|<5>)");
re!(SEVERITY_INFO, r"^\s*(I:|I/|I \(|<6>|INFO:)");
re!(SEVERITY_DEBUG, r"^\s*(D:|D/|D \(|<7>|VERBOSE:|DEBUG:)");

// assert-keyword + `<path>:<line>` (or "file ... line ...") co-occurrence.
re!(
    ASSERT_LOC,
    r"(?i)assert[^\n]*?([\w/.\-]+\.(?:c|h|cc|cpp|rs|S|py):\d+|file\s*:?\s*\S+\s+line\s*:?\s*\d+)"
);
re!(FILE_LINE, r"[\w/.\-]+\.(?:c|h|cc|cpp|rs|S|py):\d+");
re!(HEX_ADDR, r"0x[0-9a-fA-F]{4,16}");

// ≥2 `<reg-name><sep>0x<hex>` pairs on one line.
re!(
    REGISTER_PAIR,
    r"(?i)\b(?:r\d{1,2}|x\d{1,2}|a\d|pc|lr|sp|fp|ip|elr(?:_el\d)?|esr(?:_el\d)?|far(?:_el\d)?|spsr(?:_el\d)?|scr_el3|sctlr(?:_el\d)?|ttbr\d?(?:_el\d)?|R[A-D]X|R[SD]I|RSP|RBP|RIP|CS|xpsr|psr|cfsr|hfsr|bfar|mmfar|mcause|mtval|mepc)\b\s*[-:=]?\s*(?:[0-9a-fA-F]{2,4}:)?(?:0x[0-9a-fA-F]+|\[<[0-9a-fA-F]+>\])"
);
re!(
    BACKTRACE_HDR,
    r"(?i)(Call [Tt]race:|Backtrace:|Call stack:|Stack:|goroutine \d+ \[|Traceback \(most recent call last\):)"
);
re!(
    BACKTRACE_FRAME,
    r#"(?xi)
      ^\s*\[<[0-9a-f]+>\]                      # [<c0102030>] arm/older kernels
    | ^\s*\#\d+\s+(pc|0x)                      # \#00 pc 0000...  (Android tombstone)
    | \b\w+\+0x[0-9a-f]+/0x[0-9a-f]+           # func+0x10/0x40
    | ^\s*0x[0-9a-f]+(:0x[0-9a-f]+)?\s*$       # bare / paired hex frames
    | ^\s*File\s+"[^"]+",\s+line\s+\d+        # python traceback frame
    "#
);
// product-name + dotted version + optional build date/hash
re!(BANNER_VERSION, r"(?i)\b(v?\d+\.\d+(\.\d+)?(-[\w.+~]+)?)\b");
re!(
    RESET_MARKER,
    r"(?i)(resetting cpu|rebooting|reboot in |[Rr]eset cause|\brst:0x|System restart|Restarting system|Halting system)"
);
re!(
    WATCHDOG,
    r"(?xi)
      \b(watchdog|wdt|wdog)\b[^\n]*\b(timeout|timed\ out|reset|bite|bark|expired|expiry|lockup|stuck)\b
    | \b(bark|bite)\b[^\n]*\b(watchdog|wdt|wdog)\b
    | \bNMI\ watchdog\b
    "
);
re!(KEY_VALUE_RUN, r"(\w+=\S*\s+){2,}\w+=\S*");
re!(COUNTDOWN_OVERWRITE, r"[\r\x08]\s*\d+\s*$");

// An explicit syslog/printk priority at the head of the message: `<6>`, with any
// bracketed prefixes a console puts in front of it (`[    7.426731]`,
// `[firmware]`, `[cpu0]`) allowed first.
re!(EXPLICIT_PRIORITY, r"^\s*(?:\[[^\]]*\]\s*)*<([0-7])>");

/// The priority a line DECLARES, if it declares one.
///
/// A `<6>` is the emitter's own classification and outranks any word in the
/// message. Report #17 (bravo, Uno Q): `[ 7.426731] <6> cpu0 KTEST fatal.oracle
/// PASS` was filed as `crit` because of the word "fatal", and a `<6>` eMMC
/// status line as `err` because of the field name `err=`. The Linux profile's
/// `^<N>` rules could not help: they are anchored to the line start and this
/// console prints its timestamp first.
pub fn explicit_priority(text: &str) -> Option<Severity> {
    let n: i64 = EXPLICIT_PRIORITY
        .captures(text)?
        .get(1)?
        .as_str()
        .parse()
        .ok()?;
    Some(Severity::from_i64(n))
}

/// Classify a line's severity using nothing but the generic tokens.
///
/// An explicit priority tag wins outright; otherwise deliberately ordered
/// most-severe-first: a line saying "FATAL: warning suppressed" is fatal, not a
/// warning.
pub fn generic_severity(text: &str) -> Severity {
    if let Some(sev) = explicit_priority(text) {
        sev
    } else if SEVERITY_FATAL.is_match(text) {
        Severity::Crit
    } else if SEVERITY_ERROR.is_match(text) {
        Severity::Err
    } else if SEVERITY_WARN.is_match(text) {
        Severity::Warn
    } else if SEVERITY_NOTICE.is_match(text) {
        Severity::Notice
    } else if SEVERITY_INFO.is_match(text) {
        Severity::Info
    } else if SEVERITY_DEBUG.is_match(text) {
        Severity::Debug
    } else {
        Severity::Unknown
    }
}

/// A line dense with `reg = 0x…` pairs is a CONTEXT-DUMP line in every dialect.
pub fn is_register_line(text: &str) -> bool {
    REGISTER_PAIR.find_iter(text).count() >= 2
}

pub fn is_backtrace_header(text: &str) -> bool {
    BACKTRACE_HDR.is_match(text)
}

pub fn is_backtrace_frame(text: &str) -> bool {
    BACKTRACE_FRAME.is_match(text)
}

pub fn is_reset_marker(text: &str) -> bool {
    RESET_MARKER.is_match(text)
}

pub fn is_watchdog(text: &str) -> bool {
    WATCHDOG.is_match(text)
}

pub fn has_key_value_run(text: &str) -> bool {
    KEY_VALUE_RUN.is_match(text)
}

/// `GARBAGE_BURST` (§A.10): sustained non-printable / invalid-UTF-8 ratio above a
/// threshold, which is what a baud mismatch looks like. Measured on raw bytes,
/// because by definition the text view of garbage is already lossy.
#[derive(Debug, Clone)]
pub struct GarbageDetector {
    window_bytes: usize,
    threshold: f64,
    window: std::collections::VecDeque<bool>,
    nonprintable: usize,
    active: bool,
    /// Bytes whose verdict is still pending because they may be the start of a
    /// multi-byte UTF-8 sequence.
    pending: Vec<u8>,
}

impl GarbageDetector {
    pub fn new(window_bytes: usize, threshold: f64) -> Self {
        Self {
            window_bytes: window_bytes.max(1),
            threshold,
            window: std::collections::VecDeque::new(),
            nonprintable: 0,
            active: false,
            pending: Vec::new(),
        }
    }

    pub fn from_config(c: &crate::config::FramerConfig) -> Self {
        Self::new(c.garbage_window_bytes, c.garbage_threshold)
    }

    /// How many bytes a UTF-8 sequence starting with `b` should have, or `None`
    /// if `b` cannot start one at all.
    fn utf8_len(b: u8) -> Option<usize> {
        match b {
            0x00..=0x7f => Some(1),
            0xc2..=0xdf => Some(2),
            0xe0..=0xef => Some(3),
            0xf0..=0xf4 => Some(4),
            // Stray continuations, and bytes that are never valid lead bytes.
            _ => None,
        }
    }

    /// Is a single ASCII byte plausible console output?
    fn ascii_ok(b: u8) -> bool {
        matches!(b, 0x20..=0x7e | b'\n' | b'\r' | b'\t' | 0x08 | 0x07 | 0x1b)
    }

    fn commit(&mut self, bad: bool, n: usize) {
        for _ in 0..n {
            self.window.push_back(bad);
            if bad {
                self.nonprintable += 1;
            }
            if self.window.len() > self.window_bytes && self.window.pop_front() == Some(true) {
                self.nonprintable -= 1;
            }
        }
    }

    /// Feed one byte, deferring its verdict until the UTF-8 sequence it may be
    /// part of resolves.
    ///
    /// A lead byte is only good if its continuations actually arrive. Accepting
    /// it optimistically is what would let a stream of `0xE2` — precisely what a
    /// wrong baud rate produces — pass as legitimate multi-byte text.
    fn feed_byte(&mut self, b: u8) {
        self.pending.push(b);
        let lead = self.pending[0];
        match Self::utf8_len(lead) {
            None => {
                self.commit(true, 1);
                self.pending.clear();
            }
            Some(1) => {
                let ok = Self::ascii_ok(lead);
                self.commit(!ok, 1);
                self.pending.clear();
            }
            Some(n) if self.pending.len() >= n => {
                let well_formed = self.pending[1..n].iter().all(|c| (0x80..=0xBF).contains(c));
                self.commit(!well_formed, n);
                let rest: Vec<u8> = self.pending[n..].to_vec();
                self.pending.clear();
                for b in rest {
                    self.feed_byte(b);
                }
            }
            Some(_) => {}
        }
    }

    /// Feed raw bytes; returns true while the burst is active.
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            self.feed_byte(b);
        }
        // Only claim a burst once the window is actually full: three stray bytes
        // after a reset are not a baud mismatch, and calling them one would make
        // `capture_state: garbage` untrustworthy.
        if self.window.len() >= self.window_bytes {
            let ratio = self.nonprintable as f64 / self.window.len() as f64;
            self.active = ratio >= self.threshold;
        } else if self.window.is_empty() {
            self.active = false;
        }
        self.active
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn ratio(&self) -> f64 {
        if self.window.is_empty() {
            0.0
        } else {
            self.nonprintable as f64 / self.window.len() as f64
        }
    }

    pub fn reset(&mut self) {
        self.window.clear();
        self.nonprintable = 0;
        self.active = false;
        self.pending.clear();
    }
}

/// Bytes a working console legitimately emits on their own: printable ASCII plus
/// the control characters consoles really use (LF, CR, TAB, BS, BEL, ESC).
///
/// High bytes are deliberately *not* included: whether one is legitimate depends
/// on the UTF-8 sequence around it, which only `GarbageDetector` can see.
pub fn is_console_printable(b: u8) -> bool {
    matches!(b, 0x20..=0x7e | b'\n' | b'\r' | b'\t' | 0x08 | 0x07 | 0x1b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_classification_is_most_severe_first() {
        assert_eq!(
            generic_severity("Kernel panic - not syncing: Attempted to kill init!"),
            Severity::Crit
        );
        assert_eq!(
            generic_severity("mmc0: error -110 whilst initialising SD card"),
            Severity::Err
        );
        assert_eq!(
            generic_severity("WARNING: CPU: 2 PID: 1 at drivers/foo.c:12"),
            Severity::Warn
        );
        assert_eq!(generic_severity("NOTICE:  BL31: v2.11"), Severity::Notice);
        assert_eq!(
            generic_severity("I/TC: OP-TEE version: 4.1"),
            Severity::Info
        );
        assert_eq!(
            generic_severity("mmc0: new high speed card"),
            Severity::Unknown
        );
    }

    /// Report #17: the emitter's own priority tag beats every word heuristic,
    /// wherever a console's timestamp puts it.
    #[test]
    fn an_explicit_priority_tag_outranks_the_words_in_the_message() {
        assert_eq!(
            generic_severity("[    7.426731] <6> cpu0 KTEST fatal.oracle PASS"),
            Severity::Info
        );
        assert_eq!(
            generic_severity(
                "[    5.306582] <6> cpu0 =00000000 int=00000002 err=00000000 pwr_status=00000000"
            ),
            Severity::Info
        );
        assert_eq!(generic_severity("<3>mmc0: timeout"), Severity::Err);
        assert_eq!(
            generic_severity("[firmware] <4> efi: no map"),
            Severity::Warn
        );
        assert_eq!(generic_severity("<0>Kernel panic"), Severity::Emerg);
        // Not at the head of the message: still just words.
        assert_eq!(
            generic_severity("mmc0: error reading <6> register"),
            Severity::Err
        );
    }

    #[test]
    fn optee_and_esp_prefix_forms_are_recognised() {
        assert_eq!(generic_severity("E/TC:0 0 tee_ta_init"), Severity::Crit);
        assert_eq!(
            generic_severity("E (1234) wifi: init failed"),
            Severity::Err
        );
        assert_eq!(generic_severity("W (1234) wifi: retrying"), Severity::Warn);
    }

    #[test]
    fn register_lines_need_two_pairs() {
        assert!(is_register_line("pc : [<80101010>]  lr : [<80101020>]"));
        assert!(is_register_line(
            "x0 = 0x0000000000000000 x1 = 0x00000000deadbeef"
        ));
        assert!(is_register_line("RIP: 0010:0xffffffff81000000 RSP: 0x1234"));
        assert!(!is_register_line("jumping to 0x80080000"));
    }

    #[test]
    fn backtrace_headers_and_frames() {
        for h in [
            "Call Trace:",
            "Call trace:",
            "Backtrace:",
            "Call stack:",
            "Traceback (most recent call last):",
        ] {
            assert!(is_backtrace_header(h), "{h}");
        }
        for f in [
            " [<c0102030>] (dump_stack)",
            " el1h_64_sync+0x64/0x68",
            "  0x00000000801020a0",
            "  #00 pc 000000000004a1b0  /system/lib64/libc.so",
        ] {
            assert!(is_backtrace_frame(f), "{f}");
        }
        assert!(!is_backtrace_frame("Starting kernel ..."));
    }

    #[test]
    fn reset_and_watchdog_attribution_are_distinct() {
        assert!(is_reset_marker("Resetting CPU ..."));
        assert!(is_reset_marker("rst:0x10 (RTCWDT_RTC_RESET),boot:0x13"));
        assert!(is_watchdog("watchdog: BUG: soft lockup - CPU#0 stuck"));
        assert!(is_watchdog("Apps watchdog bark! Barking at 0x1234"));
        assert!(!is_watchdog("Resetting CPU ..."));
    }

    #[test]
    fn garbage_burst_needs_a_full_window() {
        let mut d = GarbageDetector::new(64, 0.30);
        // A few stray bytes after a reset must not trip it.
        assert!(!d.push(&[0xff, 0xfe, 0x00]));
        // Sustained noise does.
        assert!(d.push(&[0x00u8; 64]));
        assert!(d.is_active());
        // …and clean text clears it again.
        d.push(&[b'a'; 128]);
        assert!(!d.is_active());
    }

    #[test]
    fn normal_console_text_is_never_garbage() {
        let mut d = GarbageDetector::new(512, 0.30);
        let text = "[    1.234567] mmc0: new high speed SDHC card at address aaaa\n".repeat(20);
        assert!(!d.push(text.as_bytes()));
        assert!(d.ratio() < 0.01);
    }

    #[test]
    fn ansi_and_utf8_are_not_garbage() {
        let mut d = GarbageDetector::new(64, 0.30);
        let text = "\x1b[1;32mgreen ✓\x1b[0m\n".repeat(20);
        assert!(!d.push(text.as_bytes()));
    }

    #[test]
    fn framing_error_noise_from_a_wrong_baud_rate_is_detected() {
        // What a mismatched baud actually produces: dense high-bit bytes that
        // are not well-formed UTF-8.
        let mut d = GarbageDetector::new(512, 0.30);
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let noise: Vec<u8> = (0..2048)
            .map(|_| {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as u8
            })
            .collect();
        assert!(d.push(&noise), "ratio was {:.2}", d.ratio());
    }

    #[test]
    fn a_truncated_utf8_sequence_counts_against_the_window() {
        let mut d = GarbageDetector::new(16, 0.30);
        // Lead bytes with no continuations: exactly what noise looks like.
        assert!(d.push(&[0xE2; 32]));
    }

    #[test]
    fn assert_locator_needs_both_halves() {
        assert!(ASSERT_LOC.is_match("ASSERT_EFI_ERROR at Dxe/Main.c:412"));
        assert!(ASSERT_LOC.is_match("assertion 'x != NULL' failed at core/kernel/panic.c:33"));
        assert!(!ASSERT_LOC.is_match("asserting dominance"));
    }

    #[test]
    fn key_value_run_needs_three_pairs() {
        assert!(has_key_value_run(
            "baudrate=115200 bootdelay=2 ipaddr=10.0.0.1"
        ));
        assert!(!has_key_value_run("setting x=1 now"));
    }
}
