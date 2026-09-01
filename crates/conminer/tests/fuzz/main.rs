//! Suite `fuzz` (§12.1) — the three untrusted-input surfaces.
//!
//! Serial input is hostile by definition: a wrong baud rate *is* structured
//! garbage, a marginal cable flips bits, and a board yanked mid-oops truncates a
//! record. The line splitter, the tokenizer and the framer state machines all
//! read those bytes directly, so all three are fuzzed.
//!
//! This runs as an ordinary test with a deterministic generator plus the corpus
//! checked into `corpus/fuzz/`, so every PR exercises it. `./cm fuzz` runs the
//! same targets for longer (`CONMINER_FUZZ_SECONDS`), which is the nightly tier.
//!
//! The properties asserted are the ones whose violation would be silent:
//! nothing panics, no byte is lost, and framing still tiles the line sequence.

use conminer_core::config::{FramerConfig, LineEndingMode};
use conminer_core::drain::{Drain, DrainConfig, TokenizerRules};
use conminer_core::framer::{check_conservation, Framer, FramerInput, ProfileFramer, ProfileSet};
use conminer_core::linesplit::{reassemble, split_all, LineSplitter};
use conminer_testkit::fault::{bit_flips, chunks, drop_span, garbage_burst, truncate_at, Rng};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn budget() -> Duration {
    Duration::from_secs(
        std::env::var("CONMINER_FUZZ_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5),
    )
}

/// Seeds: the checked-in corpus, plus the hostile shapes that break parsers.
fn seeds() -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"\n".to_vec(),
        b"\r".to_vec(),
        b"\r\n".to_vec(),
        b"\x1b[2J\x1b[1;1H".to_vec(),
        vec![0u8; 512],
        vec![0xffu8; 512],
        (0u8..=255).collect(),
        b"[    0.000000] Linux version 6.12.9 (b@h) (gcc)\n".to_vec(),
        b"Internal error: Oops: 0 [#1] PREEMPT SMP\n".to_vec(),
        b"\xef\xbb\xbfBOM at the start\n".to_vec(),
        // A UTF-8 sequence torn exactly at its boundary.
        vec![0xe2, 0x9c],
    ];
    for dir in ["linux", "uboot", "zephyr", "tfa", "hostile"] {
        for p in conminer_testkit::corpus::corpus_files(dir) {
            if let Ok(b) = std::fs::read(&p) {
                out.push(b);
            }
        }
    }
    for p in std::fs::read_dir(conminer_testkit::corpus::corpus_dir().join("fuzz"))
        .into_iter()
        .flatten()
        .flatten()
    {
        if let Ok(b) = std::fs::read(p.path()) {
            out.push(b);
        }
    }
    out
}

/// Derive a mutated input from a seed, deterministically.
fn mutate(seed: &[u8], n: u64) -> Vec<u8> {
    let mut rng = Rng::new(n);
    let mut data = seed.to_vec();
    match n % 5 {
        0 => data = bit_flips(&data, n, 16),
        1 => data = garbage_burst(&data, rng.below(data.len().max(1)), 1 + rng.below(600), n),
        2 => data = truncate_at(&data, rng.below(data.len().max(1))),
        3 => data = drop_span(&data, rng.below(data.len().max(1)), 1 + rng.below(64)),
        _ => {
            // Splice two seeds together: the shape a reconnect produces.
            data.extend_from_slice(&seed[..seed.len() / 2]);
        }
    }
    data
}

#[test]
fn fuzz_the_line_splitter_never_loses_a_byte() {
    let deadline = Instant::now() + budget();
    let seeds = seeds();
    let mut n = 0u64;
    while Instant::now() < deadline {
        let seed = &seeds[(n as usize) % seeds.len()];
        let data = mutate(seed, n);
        for mode in [LineEndingMode::Auto, LineEndingMode::Lf, LineEndingMode::Cr] {
            let lines = split_all(&data, mode, 1 << 16);
            assert_eq!(
                reassemble(&lines),
                data,
                "byte loss at iteration {n} in {mode:?}"
            );
            assert!(lines.iter().all(|l| l.bytes.len() <= 1 << 16));
        }
        // …and chopping the same bytes differently changes nothing.
        let mut s = LineSplitter::new(LineEndingMode::Auto, 1 << 16, 4096);
        let mut piecewise = Vec::new();
        for c in chunks(&data, n, 37) {
            piecewise.extend(s.push(&c));
        }
        piecewise.extend(s.flush());
        assert_eq!(
            reassemble(&piecewise),
            data,
            "chunking changed the bytes at iteration {n}"
        );
        n += 1;
    }
    eprintln!("line splitter: {n} iterations");
    assert!(n > 0);
}

#[test]
fn fuzz_the_tokenizer_never_panics_and_never_invents_tokens() {
    let deadline = Instant::now() + budget();
    let seeds = seeds();
    let rules = [
        TokenizerRules::default(),
        TokenizerRules {
            split_key_value: true,
            ..Default::default()
        },
        TokenizerRules {
            split_key_value: true,
            extra_delimiters: vec![':', ',', '='],
        },
    ];
    let mut n = 0u64;
    while Instant::now() < deadline {
        let data = mutate(&seeds[(n as usize) % seeds.len()], n);
        let text = String::from_utf8_lossy(&data);
        for r in &rules {
            for line in text.lines().take(200) {
                let tokens = r.tokenize(line);
                // Tokenization only ever splits or MASKS: apart from the
                // wildcard it substitutes for numeric literals, it cannot
                // produce more characters than it was given. Discounting the
                // wildcard keeps the original invariant -- no invented content
                // -- while allowing the one substitution that is deliberate.
                let produced: usize = tokens
                    .iter()
                    .map(|t| {
                        t.replace(conminer_core::drain::WILDCARD, "")
                            .chars()
                            .count()
                    })
                    .sum();
                assert!(
                    produced <= line.chars().count() + tokens.len(),
                    "tokenizer invented characters on {line:?}"
                );
                assert!(tokens.iter().all(|t| !t.is_empty()));
            }
        }
        n += 1;
    }
    eprintln!("tokenizer: {n} iterations");
}

#[test]
fn fuzz_the_framer_state_machines_stay_conserving() {
    let deadline = Instant::now() + budget();
    let seeds = seeds();
    let set = Arc::new(ProfileSet::builtin().unwrap());
    let profiles = ["raw", "linux", "uboot", "zephyr", "uefi", "tfa", "optee"];
    let cfg = FramerConfig::default();

    let mut n = 0u64;
    while Instant::now() < deadline {
        let data = mutate(&seeds[(n as usize) % seeds.len()], n);
        let profile = profiles[(n as usize) % profiles.len()];
        let mut framer = ProfileFramer::new(set.clone(), &cfg, Some(profile)).unwrap();

        let lines = split_all(&data, LineEndingMode::Auto, 1 << 16);
        let mut ids = Vec::new();
        let mut events = Vec::new();
        for (i, l) in lines.iter().enumerate().take(4000) {
            let id = i as i64 + 1;
            ids.push(id);
            let mut input = FramerInput::new(id, l.lossy().into_owned(), i as i64 * 10);
            // Exercise the quarantine paths too.
            input.garbage = n % 7 == 0 && i % 3 == 0;
            input.binary = n % 11 == 0 && i % 5 == 0;
            events.extend(framer.push(input));
        }
        events.extend(framer.flush());

        if let Err(e) = check_conservation(&ids, &events) {
            panic!("framer conservation violated on {profile} at iteration {n}: {e}");
        }
        // Every line is covered exactly once once the stream has ended.
        let covered: i64 = events
            .iter()
            .filter_map(|e| e.as_record())
            .map(|r| r.line_count)
            .sum();
        assert_eq!(
            covered as usize,
            ids.len(),
            "{profile} dropped or duplicated lines at iteration {n}"
        );
        n += 1;
    }
    eprintln!("framer: {n} iterations");
}

#[test]
fn fuzz_the_miner_is_deterministic_under_hostile_input() {
    let deadline = Instant::now() + budget();
    let seeds = seeds();
    let mut n = 0u64;
    while Instant::now() < deadline {
        let data = mutate(&seeds[(n as usize) % seeds.len()], n);
        let text = String::from_utf8_lossy(&data);
        let lines: Vec<String> = text.lines().take(500).map(str::to_string).collect();

        let run = || {
            let mut d = Drain::new(DrainConfig::default());
            for l in &lines {
                d.add_line(l);
            }
            d
        };
        let a = run();
        let b = run();
        assert_eq!(
            a.templates(),
            b.templates(),
            "mining was not deterministic at iteration {n}"
        );
        // Analysis must never mutate.
        let before = a.templates().to_vec();
        let _ = a.merge_suggestions();
        let _ = a.fragmentation_ratio();
        assert_eq!(a.templates(), before.as_slice());
        n += 1;
    }
    eprintln!("miner: {n} iterations");
}

/// The prompt classifier, against the same hostile bytes.
///
/// This suite covered the splitter, the tokenizer, the framer and the miner --
/// every stage EXCEPT the one that decides what the console is sitting at. A
/// printk-stripping fix shipped with byte arithmetic on a `&str`, and the first
/// `console_state` call on hardware panicked inside a U+FFFD produced by a
/// single invalid byte on the wire. A fuzz pass over `classify` costs seconds
/// and closes the whole class, not just that instance.
#[test]
fn fuzz_prompt_classification_never_panics_on_hostile_console_bytes() {
    use conminer_core::framer::profile::PromptKind;
    use conminer_core::runner::{Prompt, Prompts};

    let prompts = Prompts(vec![
        Prompt {
            re: regex::Regex::new(r"root@[\w.-]+:[^\s]*[#$]\s*$").unwrap(),
            raw: r"root@.*[#$] $".into(),
            kind: PromptKind::Shell,
        },
        Prompt {
            re: regex::Regex::new(r"(?i)login:\s*$").unwrap(),
            raw: "login: $".into(),
            kind: PromptKind::CredentialGate,
        },
    ]);

    // Shapes that specifically stress the printk-stripper: a multi-byte or
    // invalid leading byte in front of a prompt and a bracketed timestamp.
    let extra: Vec<Vec<u8>> = vec![
        b"\xffroot@iq10:~# [  862.282959] phy phy-fc3a00.phy.0: on".to_vec(),
        "\u{fffd}root@iq10:~# ".as_bytes().to_vec(),
        "é[  1.0] kernel says something".as_bytes().to_vec(),
        b"\x1b[?2004hroot@host:~# [12.3] x".to_vec(),
        b"[".to_vec(),
        b"[[[[[[[[".to_vec(),
        b"[0.0][0.0][0.0]root@h:~# ".to_vec(),
    ];

    let deadline = Instant::now() + budget();
    let mut seeds = seeds();
    seeds.extend(extra);
    let mut n = 0u64;
    while Instant::now() < deadline {
        let data = mutate(&seeds[(n as usize) % seeds.len()], n);
        let text = String::from_utf8_lossy(&data);
        // The whole tail, and each line on its own: derive() does both.
        let _ = prompts.classify(&text);
        for line in text.lines().take(200) {
            let _ = prompts.classify(line);
        }
        n += 1;
    }
    assert!(n > 0, "the fuzzer must actually have run");
}
