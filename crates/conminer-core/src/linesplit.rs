//! Byte stream → line records (§13 `linesplit`).
//!
//! This is the first stage of the pipeline and the one that carries the
//! no-masking guarantee (§6): **bytes go in and come back out unchanged**. The
//! splitter decides only where the boundaries are; it never rewrites content.
//!
//! Executable form of that guarantee, checked as a property test:
//! concatenating every emitted line's `bytes` followed by its terminator's raw
//! bytes reproduces the input stream exactly — including invalid UTF-8, NULs,
//! ANSI escapes and torn codepoints.
//!
//! Line endings are genuinely ambiguous on serial consoles: `\r` is both a line
//! terminator (classic consoles) and an overwrite control (U-Boot autoboot
//! countdowns, UEFI progress spinners). `LineEndingMode::Auto` resolves this by
//! probing — it locks to LF the moment any `\n` appears, and to CR only if the
//! probe window passes with carriage returns and no linefeed at all.

use crate::config::LineEndingMode;

/// What ended a line. `raw()` gives back the exact bytes consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    Lf,
    CrLf,
    Cr,
    /// End of stream, or a forced cut at `max_line_bytes`. No bytes consumed.
    None,
}

impl Terminator {
    pub fn raw(self) -> &'static [u8] {
        match self {
            Terminator::Lf => b"\n",
            Terminator::CrLf => b"\r\n",
            Terminator::Cr => b"\r",
            Terminator::None => b"",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Terminator::Lf => "lf",
            Terminator::CrLf => "crlf",
            Terminator::Cr => "cr",
            Terminator::None => "none",
        }
    }

    /// The inverse of [`as_str`], for reading a stored row back.
    ///
    /// THE COLUMN HOLDS THE NAME, NOT THE BYTES. `raw_lines.terminator` stores
    /// "lf"/"crlf"/"cr"/"none", so `length(terminator)` in SQL is the length of
    /// the LABEL (2, 4, 2, 4) and not of what the board actually sent (1, 2, 1,
    /// 0). Anything reconstructing a stream offset from a stored row has to come
    /// back through here.
    pub fn from_label(label: &str) -> Self {
        match label {
            "lf" => Terminator::Lf,
            "crlf" => Terminator::CrLf,
            "cr" => Terminator::Cr,
            _ => Terminator::None,
        }
    }
}

/// One line, verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Content bytes, excluding the terminator. Never modified.
    pub bytes: Vec<u8>,
    pub terminator: Terminator,
    /// Cut at `max_line_bytes`; the next line is its continuation.
    pub truncated: bool,
    /// This line continues a previously truncated one.
    pub continuation: bool,
}

impl Line {
    /// UTF-8-lossy display view (§16 `capture.encoding`: "bytes stored verbatim
    /// always; display view replaces invalid sequences").
    pub fn lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }

    /// Display text after applying CR/backspace overwrite, so a U-Boot autoboot
    /// countdown renders as its final state while `bytes` stays raw.
    pub fn rendered(&self) -> String {
        render_overwrites(&self.lossy())
    }

    /// Total bytes this line consumed from the stream, terminator included.
    pub fn consumed(&self) -> usize {
        self.bytes.len() + self.terminator.raw().len()
    }
}

/// Apply `\r` (column 0) and `\x08` (backspace) as a terminal would, so
/// rewritten fields collapse to their final text.
pub fn render_overwrites(s: &str) -> String {
    let mut out: Vec<char> = Vec::with_capacity(s.len());
    let mut col = 0usize;
    for c in s.chars() {
        match c {
            '\r' => col = 0,
            '\u{8}' => col = col.saturating_sub(1),
            _ => {
                if col < out.len() {
                    out[col] = c;
                } else {
                    out.push(c);
                }
                col += 1;
            }
        }
    }
    out.into_iter().collect()
}

/// Incremental byte→line splitter. Feed it arbitrary chunks; boundaries never
/// depend on how the stream was chopped up.
#[derive(Debug)]
pub struct LineSplitter {
    mode: LineEndingMode,
    /// Still deciding between LF and CR (`Auto` only).
    probing: bool,
    probe_budget: usize,
    probe_consumed: usize,
    max_line_bytes: usize,

    buf: Vec<u8>,
    /// A `\r` was seen and we don't yet know whether `\n` follows.
    pending_cr: bool,
    /// The next emitted line continues a truncated one.
    next_is_continuation: bool,

    out: Vec<Line>,
    /// Total bytes fed in, for offset accounting.
    bytes_in: u64,
}

impl LineSplitter {
    pub fn new(mode: LineEndingMode, max_line_bytes: usize, probe_bytes: usize) -> Self {
        Self {
            mode,
            probing: mode == LineEndingMode::Auto,
            probe_budget: probe_bytes.max(1),
            probe_consumed: 0,
            max_line_bytes: max_line_bytes.max(64),
            buf: Vec::with_capacity(256),
            pending_cr: false,
            next_is_continuation: false,
            out: Vec::new(),
            bytes_in: 0,
        }
    }

    pub fn with_config(c: &crate::config::CaptureConfig) -> Self {
        Self::new(
            c.line_ending_mode,
            c.max_line_bytes,
            c.line_ending_probe_bytes,
        )
    }

    /// The mode currently in force. During `Auto` probing this reports `Auto`.
    pub fn mode(&self) -> LineEndingMode {
        if self.probing {
            LineEndingMode::Auto
        } else {
            self.mode
        }
    }

    pub fn bytes_in(&self) -> u64 {
        self.bytes_in
    }

    /// Bytes buffered in an unterminated line. Live callers use this to decide
    /// whether a "quiet" console really has nothing pending.
    pub fn pending_bytes(&self) -> usize {
        self.buf.len() + usize::from(self.pending_cr)
    }

    /// The text of the unterminated line, as the console is showing it now.
    ///
    /// A shell prompt has NO terminator -- that is the whole point of a prompt:
    /// the cursor stays on the line waiting for input. So a board sitting at
    /// `root@iq10:~#` has that text HERE and nowhere else: it is not in
    /// `raw_lines`, and it will not be until either the operator presses enter
    /// or `framer.record_timeout_s` (10 s) of dead air forces the record closed.
    ///
    /// This cost four rounds of the same finding. `console_state` classified the
    /// prompt from stored lines only, found kernel chatter and no prompt, fell
    /// through to epoch-chain analysis and answered `unstable, commandable:
    /// false` at a perfectly healthy idle root shell -- while `run_command`,
    /// which asserts the prompt on the wire instead of reading the store, drove
    /// that same console without trouble.
    pub fn pending_text(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }

    /// Feed a chunk; returns every line completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Line> {
        self.bytes_in += chunk.len() as u64;
        for &b in chunk {
            self.byte(b);
        }
        std::mem::take(&mut self.out)
    }

    /// End of stream: emit any partial line with `Terminator::None`.
    pub fn flush(&mut self) -> Vec<Line> {
        if self.pending_cr {
            self.pending_cr = false;
            if self.cr_terminates() {
                self.emit(Terminator::Cr);
            } else {
                self.buf.push(b'\r');
            }
        }
        if !self.buf.is_empty() {
            self.emit(Terminator::None);
        }
        std::mem::take(&mut self.out)
    }

    fn cr_terminates(&self) -> bool {
        self.mode == LineEndingMode::Cr && !self.probing
    }

    fn byte(&mut self, b: u8) {
        if self.probing {
            self.probe_consumed += 1;
        }

        if self.pending_cr {
            self.pending_cr = false;
            if b == b'\n' {
                // CRLF: one terminator, and proof the stream uses linefeeds.
                self.lock_lf();
                self.emit(Terminator::CrLf);
                return;
            }
            if self.cr_terminates() {
                self.emit(Terminator::Cr);
            } else {
                // Bare CR kept verbatim as an overwrite control.
                self.push_content(b'\r');
            }
            // fall through and handle `b` normally
        }

        match b {
            b'\r' => {
                self.pending_cr = true;
                self.maybe_end_probe(true);
            }
            b'\n' => {
                self.lock_lf();
                self.emit(Terminator::Lf);
            }
            _ => {
                self.push_content(b);
                self.maybe_end_probe(false);
            }
        }
    }

    fn push_content(&mut self, b: u8) {
        self.buf.push(b);
        if self.buf.len() >= self.max_line_bytes {
            // Forced cut: no bytes invented, no bytes lost — the next line is
            // flagged as this one's continuation.
            self.emit_flagged(Terminator::None, true);
        }
    }

    fn lock_lf(&mut self) {
        if self.probing {
            self.probing = false;
            if self.mode == LineEndingMode::Auto {
                self.mode = LineEndingMode::Lf;
            }
        }
    }

    fn maybe_end_probe(&mut self, saw_cr: bool) {
        if !self.probing {
            return;
        }
        let budget_spent = self.probe_consumed >= self.probe_budget;
        if !budget_spent {
            return;
        }
        self.probing = false;
        let cr_in_buf = saw_cr || self.buf.contains(&b'\r');
        if cr_in_buf {
            // Carriage returns, never a linefeed: a `\r`-only console.
            self.mode = LineEndingMode::Cr;
            self.resplit_buffered_crs();
        } else {
            self.mode = LineEndingMode::Lf;
        }
    }

    /// When `Auto` resolves to CR *after* buffering CRs as overwrite controls,
    /// retroactively split on them. Byte-preserving: each `\r` becomes a
    /// `Terminator::Cr` whose `raw()` puts it back.
    fn resplit_buffered_crs(&mut self) {
        if !self.buf.contains(&b'\r') {
            return;
        }
        let buf = std::mem::take(&mut self.buf);
        let mut segments = buf.split(|&b| b == b'\r').peekable();
        while let Some(seg) = segments.next() {
            if segments.peek().is_some() {
                self.buf = seg.to_vec();
                self.emit(Terminator::Cr);
            } else {
                self.buf = seg.to_vec();
            }
        }
    }

    fn emit(&mut self, t: Terminator) {
        self.emit_flagged(t, false);
    }

    fn emit_flagged(&mut self, t: Terminator, truncated: bool) {
        let bytes = std::mem::take(&mut self.buf);
        let continuation = self.next_is_continuation;
        self.next_is_continuation = truncated;
        self.out.push(Line {
            bytes,
            terminator: t,
            truncated,
            continuation,
        });
    }
}

/// Convenience: split a whole buffer in one shot (post-hoc ingest, tests).
pub fn split_all(data: &[u8], mode: LineEndingMode, max_line_bytes: usize) -> Vec<Line> {
    let mut s = LineSplitter::new(mode, max_line_bytes, 4096);
    let mut lines = s.push(data);
    lines.extend(s.flush());
    lines
}

/// Reassemble the exact input bytes from split lines. Used by the raw-preservation
/// property test and by `export_session`.
pub fn reassemble(lines: &[Line]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in lines {
        out.extend_from_slice(&l.bytes);
        out.extend_from_slice(l.terminator.raw());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LineEndingMode::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.lossy().into_owned()).collect()
    }

    #[test]
    fn lf_crlf_and_mixed_in_one_stream() {
        let input = b"alpha\nbravo\r\ncharlie\ndelta\r\n";
        let lines = split_all(input, Auto, 1 << 20);
        assert_eq!(texts(&lines), ["alpha", "bravo", "charlie", "delta"]);
        assert_eq!(lines[0].terminator, Terminator::Lf);
        assert_eq!(lines[1].terminator, Terminator::CrLf);
        assert_eq!(reassemble(&lines), input);
    }

    #[test]
    fn cr_only_stream_locks_to_cr_and_splits() {
        // No linefeed anywhere: probe budget expires and CR becomes the terminator.
        let input = b"one\rtwo\rthree\r";
        let lines = {
            let mut s = LineSplitter::new(Auto, 1 << 20, 8);
            let mut v = s.push(input);
            v.extend(s.flush());
            v
        };
        assert_eq!(texts(&lines), ["one", "two", "three"]);
        assert!(lines.iter().all(|l| l.terminator == Terminator::Cr));
        assert_eq!(reassemble(&lines), input);
    }

    #[test]
    fn bare_cr_is_an_overwrite_control_when_the_stream_uses_lf() {
        let input = b"progress: 10%\rprogress: 90%\rprogress: 100%\n";
        let lines = split_all(input, Auto, 1 << 20);
        assert_eq!(lines.len(), 1, "CR must not split an LF-terminated stream");
        assert_eq!(lines[0].rendered(), "progress: 100%");
        assert_eq!(reassemble(&lines), input);
    }

    #[test]
    fn uboot_countdown_renders_as_final_text_bytes_preserved() {
        let input =
            b"Hit any key to stop autoboot:  3 \x08\x08\x08 2 \x08\x08\x08 1 \x08\x08\x08 0 \n";
        let lines = split_all(input, Auto, 1 << 20);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].rendered(), "Hit any key to stop autoboot:  0 ");
        // The raw bytes still carry every backspace.
        assert_eq!(lines[0].bytes.iter().filter(|&&b| b == 8).count(), 9);
        assert_eq!(reassemble(&lines), input);
    }

    #[test]
    fn no_trailing_newline_still_emits_the_line() {
        let input = b"tail with no newline";
        let lines = split_all(input, Auto, 1 << 20);
        assert_eq!(texts(&lines), ["tail with no newline"]);
        assert_eq!(lines[0].terminator, Terminator::None);
        assert_eq!(reassemble(&lines), input);
    }

    #[test]
    fn null_bytes_and_invalid_utf8_survive_verbatim() {
        let input: &[u8] = b"pre\x00mid\xff\xfe post\nnext\n";
        let lines = split_all(input, Auto, 1 << 20);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].bytes.contains(&0));
        assert!(lines[0].bytes.contains(&0xff));
        // Lossy view is a *view*; it never replaces the stored bytes.
        assert!(lines[0].lossy().contains('\u{fffd}'));
        assert_eq!(reassemble(&lines), input);
    }

    #[test]
    fn oversized_line_is_cut_with_a_continuation_flag() {
        let mut input = vec![b'x'; 300];
        input.push(b'\n');
        let lines = split_all(&input, Auto, 128);
        assert_eq!(lines.len(), 3, "128 + 128 + the 44-byte remainder");
        assert!(lines[0].truncated && !lines[0].continuation);
        assert!(lines[1].truncated && lines[1].continuation);
        assert!(!lines[2].truncated && lines[2].continuation);
        assert_eq!(lines[2].terminator, Terminator::Lf);
        assert_eq!(reassemble(&lines), input);
    }

    #[test]
    fn ansi_escapes_are_content_not_structure() {
        let input = "\x1b[2J\x1b[1;32mgreen\x1b[0m\n".as_bytes();
        let lines = split_all(input, Auto, 1 << 20);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].bytes.starts_with(b"\x1b[2J"));
        assert_eq!(reassemble(&lines), input);
    }

    #[test]
    fn split_mid_codepoint_and_mid_crlf_across_reads() {
        let input = "héllo wörld\r\nsecond ✓ line\n".as_bytes().to_vec();
        for cut in 1..input.len() {
            let mut s = LineSplitter::new(Auto, 1 << 20, 4096);
            let mut lines = s.push(&input[..cut]);
            lines.extend(s.push(&input[cut..]));
            lines.extend(s.flush());
            assert_eq!(
                texts(&lines),
                ["héllo wörld", "second ✓ line"],
                "cut at {cut}"
            );
            assert_eq!(reassemble(&lines), input, "cut at {cut}");
        }
    }

    #[test]
    fn forced_modes_are_honoured() {
        let input = b"a\rb\rc";
        assert_eq!(texts(&split_all(input, Cr, 1 << 20)), ["a", "b", "c"]);
        assert_eq!(texts(&split_all(input, Lf, 1 << 20)), ["a\rb\rc"]);
    }

    #[test]
    fn empty_lines_are_real_lines() {
        let input = b"\n\n\r\n";
        let lines = split_all(input, Auto, 1 << 20);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.bytes.is_empty()));
        assert_eq!(reassemble(&lines), input);
    }
}
