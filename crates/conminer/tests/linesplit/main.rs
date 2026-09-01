//! Suite `linesplit` (§13) — byte→line, plus the §12.1 raw-preservation and
//! chunking-independence properties.
//!
//! Edge cases from the catalog, each at least one test:
//!   \n · \r\n · \r-only · mixed within one stream · no trailing newline ·
//!   1 MB single line (cap + continuation flag) · null bytes · invalid UTF-8 ·
//!   split mid-codepoint across reads · ANSI escapes · backspace/CR overwrite
//!   sequences (U-Boot countdown renders as final text, bytes preserved raw)

use conminer_core::config::LineEndingMode::{self, Auto, Cr, Lf};
use conminer_core::linesplit::{reassemble, split_all, LineSplitter, Terminator};
use proptest::prelude::*;

const BIG: usize = 1 << 20;

fn feed_in_chunks(data: &[u8], chunks: &[usize], mode: LineEndingMode, cap: usize) -> Vec<u8> {
    let mut s = LineSplitter::new(mode, cap, 4096);
    let mut lines = Vec::new();
    let mut pos = 0;
    for &n in chunks {
        if pos >= data.len() {
            break;
        }
        let end = (pos + n.max(1)).min(data.len());
        lines.extend(s.push(&data[pos..end]));
        pos = end;
    }
    if pos < data.len() {
        lines.extend(s.push(&data[pos..]));
    }
    lines.extend(s.flush());
    reassemble(&lines)
}

// ---------------------------------------------------------------- endings ----

#[test]
fn lf_endings() {
    let l = split_all(b"one\ntwo\nthree\n", Auto, BIG);
    assert_eq!(l.len(), 3);
    assert!(l.iter().all(|x| x.terminator == Terminator::Lf));
}

#[test]
fn crlf_endings() {
    let l = split_all(b"one\r\ntwo\r\n", Auto, BIG);
    assert_eq!(l.len(), 2);
    assert!(l.iter().all(|x| x.terminator == Terminator::CrLf));
    assert_eq!(l[0].lossy(), "one");
}

#[test]
fn cr_only_endings() {
    let mut s = LineSplitter::new(Auto, BIG, 4);
    let mut l = s.push(b"one\rtwo\rthree\r");
    l.extend(s.flush());
    assert_eq!(l.len(), 3);
    assert!(l.iter().all(|x| x.terminator == Terminator::Cr));
}

#[test]
fn mixed_endings_within_one_stream() {
    let input = b"lf\ncrlf\r\nlf again\ncrlf again\r\n";
    let l = split_all(input, Auto, BIG);
    let kinds: Vec<_> = l.iter().map(|x| x.terminator).collect();
    assert_eq!(
        kinds,
        [
            Terminator::Lf,
            Terminator::CrLf,
            Terminator::Lf,
            Terminator::CrLf
        ]
    );
    assert_eq!(reassemble(&l), input);
}

#[test]
fn no_trailing_newline() {
    let l = split_all(b"complete\npartial", Auto, BIG);
    assert_eq!(l.len(), 2);
    assert_eq!(l[1].terminator, Terminator::None);
    assert_eq!(l[1].lossy(), "partial");
}

// ------------------------------------------------------------------- caps ----

#[test]
fn one_megabyte_single_line_is_capped_with_continuation_flags() {
    let mut input = vec![b'A'; BIG + 4096];
    input.push(b'\n');
    let l = split_all(&input, Auto, BIG);

    assert_eq!(l.len(), 2);
    assert!(l[0].truncated, "the cut line is flagged truncated");
    assert!(!l[0].continuation);
    assert!(l[1].continuation, "the remainder is flagged a continuation");
    assert!(!l[1].truncated);
    assert_eq!(l[0].bytes.len(), BIG);
    assert_eq!(reassemble(&l), input, "a cap must never lose bytes");
}

#[test]
fn cap_applies_repeatedly_to_a_runaway_line() {
    let input = vec![b'z'; 1000];
    let l = split_all(&input, Auto, 100);
    assert_eq!(l.len(), 10);
    assert!(l[..9].iter().all(|x| x.truncated));
    assert!(l[1..].iter().all(|x| x.continuation));
    assert_eq!(reassemble(&l), input);
}

// ------------------------------------------------------------ hostile bytes --

#[test]
fn null_bytes_are_content() {
    let input = b"before\x00\x00after\nnext\n";
    let l = split_all(input, Auto, BIG);
    assert_eq!(l.len(), 2);
    assert_eq!(l[0].bytes.iter().filter(|&&b| b == 0).count(), 2);
    assert_eq!(reassemble(&l), input);
}

#[test]
fn invalid_utf8_is_stored_verbatim_and_only_the_view_is_lossy() {
    let input: &[u8] = &[0xC3, 0x28, 0xA0, 0xA1, b'x', b'\n'];
    let l = split_all(input, Auto, BIG);
    assert_eq!(l[0].bytes, &input[..input.len() - 1]);
    assert!(l[0].lossy().contains('\u{FFFD}'));
    assert_eq!(reassemble(&l), input);
}

#[test]
fn split_mid_codepoint_across_reads_changes_nothing() {
    let input = "ünïcödé ✓ line\nsecond ✗ line\n".as_bytes();
    let whole = split_all(input, Auto, BIG);
    for cut in 1..input.len() {
        let mut s = LineSplitter::new(Auto, BIG, 4096);
        let mut l = s.push(&input[..cut]);
        l.extend(s.push(&input[cut..]));
        l.extend(s.flush());
        assert_eq!(l, whole, "cut at byte {cut}");
    }
}

#[test]
fn ansi_escape_sequences_pass_through_untouched() {
    let input = "\x1b[2J\x1b[H\x1b[1;31mERROR\x1b[0m: it broke\n".as_bytes();
    let l = split_all(input, Auto, BIG);
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].bytes, &input[..input.len() - 1]);
    assert_eq!(reassemble(&l), input);
}

// ------------------------------------------------------- overwrite renders ---

#[test]
fn uboot_autoboot_countdown_renders_as_final_text() {
    let raw = b"Hit any key to stop autoboot:  3 \x08\x08\x08 2 \x08\x08\x08 1 \x08\x08\x08 0 \n";
    let l = split_all(raw, Auto, BIG);
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].rendered(), "Hit any key to stop autoboot:  0 ");
    assert_eq!(reassemble(&l), raw, "render is a view; bytes stay raw");
}

#[test]
fn cr_progress_spinner_renders_as_its_last_frame() {
    let raw = b"Copying 10%\rCopying 55%\rCopying 100%\n";
    let l = split_all(raw, Auto, BIG);
    assert_eq!(l.len(), 1, "CR must not split an LF stream");
    assert_eq!(l[0].rendered(), "Copying 100%");
    assert_eq!(reassemble(&l), raw);
}

#[test]
fn forced_cr_mode_treats_the_same_bytes_as_separate_lines() {
    let raw = b"Copying 10%\rCopying 55%\rCopying 100%\n";
    let l = split_all(raw, Cr, BIG);
    assert_eq!(l.len(), 3);
    assert_eq!(reassemble(&l), raw);
}

// -------------------------------------------------------------- properties ---

proptest! {
    /// §12.1 raw preservation: for any byte sequence x, read(store(x)) == x.
    /// This is the no-masking guarantee as an executable property.
    #[test]
    fn prop_raw_preservation(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        for mode in [Auto, Lf, Cr] {
            let l = split_all(&data, mode, BIG);
            prop_assert_eq!(reassemble(&l), data.clone(), "mode {:?}", mode);
        }
    }

    /// §12.1 chunking independence: splitting the input byte stream at arbitrary
    /// boundaries (mid-line, mid-codepoint, mid-escape) never changes the output.
    #[test]
    fn prop_chunking_independence(
        data in proptest::collection::vec(any::<u8>(), 0..2048),
        chunks in proptest::collection::vec(1usize..64, 1..64),
    ) {
        let whole = split_all(&data, Auto, BIG);
        let chunked = feed_in_chunks(&data, &chunks, Auto, BIG);
        prop_assert_eq!(chunked, reassemble(&whole));
    }

    /// Line boundaries themselves are chunk-independent, not just the bytes.
    #[test]
    fn prop_boundaries_are_chunk_independent(
        text in "(?s)[a-z \r\n\t\\x08\\x1b]{0,512}",
        chunk in 1usize..17,
    ) {
        let data = text.as_bytes();
        let whole = split_all(data, Auto, BIG);
        let mut s = LineSplitter::new(Auto, BIG, 4096);
        let mut piecewise = Vec::new();
        for c in data.chunks(chunk) {
            piecewise.extend(s.push(c));
        }
        piecewise.extend(s.flush());
        prop_assert_eq!(piecewise, whole);
    }

    /// The cap never loses or invents a byte, at any cap size.
    #[test]
    fn prop_cap_is_lossless(
        data in proptest::collection::vec(any::<u8>(), 0..2048),
        cap in 64usize..512,
    ) {
        let l = split_all(&data, Auto, cap);
        prop_assert_eq!(reassemble(&l), data.clone());
        prop_assert!(l.iter().all(|x| x.bytes.len() <= cap));
    }

    /// Rendering is idempotent and never longer than its input.
    #[test]
    fn prop_render_is_a_view(text in "[a-zA-Z0-9 \r\\x08]{0,256}") {
        let once = conminer_core::linesplit::render_overwrites(&text);
        let twice = conminer_core::linesplit::render_overwrites(&once);
        prop_assert_eq!(&once, &twice);
        prop_assert!(once.chars().count() <= text.chars().count());
    }
}
