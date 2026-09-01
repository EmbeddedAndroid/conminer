//! Suite `provenance` (§18.5) — is this the image you think it is?
//!
//! The failure this guards against is not exotic: a flash that did not take, and
//! an afternoon spent debugging the previous image. Every conclusion drawn from
//! such a boot is about the wrong build, so the check has to be cheap and it has
//! to be loud.
//!
//! The rule that matters most: a mismatch is only claimable when *both* sides
//! are known. "I cannot tell" is a third answer and must never read as "they
//! agree", because a false all-clear here is worse than no check at all.

use conminer_testkit::McpRig;
use serde_json::json;

fn boot_with(version: &str) -> String {
    format!(
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         U-Boot {version} (Jan 04 2026 - 12:00:11 +0000)\n\
         [    3.000000] Linux version 6.12.9 (build@lab) (gcc 14.2.0) #1 SMP\n"
    )
}

/// Claim that `image` is what should be on the board.
///
/// Uses `set_image`, the by-hand path, precisely because most bring-up labs
/// flash by hand: provenance has to be checkable without a configured hook.
/// Bind an image BY HAND. Named for what it does: `set_image` records what
/// somebody says is on the board, which is not evidence that bytes were pushed
/// to it. It was called `note_flash`, and that name is exactly the confusion
/// §G5 fixed in the verdict prose.
fn bind_by_hand(rig: &McpRig, device: &str, image: &str) {
    rig.call("acquire", json!({"device": device}));
    rig.call("set_image", json!({"device": device, "name": image}));
}

#[test]
fn the_running_build_is_read_off_the_console() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_with("2026.01"), None);
    let p = rig.call("provenance", json!({"device": device}));
    // §F2 restructured this: `running` is now keyed by COMPONENT and carries
    // what the banner said (version, build, builder, and the line it came from)
    // rather than a flat name→string map. The flat view is kept as
    // `running_versions` for callers that only want the strings.
    assert_eq!(
        p["running"]["uboot"]["version"], "2026.01",
        "the banner is where a board says what it is: {p:#}"
    );
    assert!(
        p["running"]["uboot"]["line_id"].is_i64(),
        "and the claim must be checkable against the console: {p:#}"
    );
    assert_eq!(
        p["running_versions"]["uboot"], "2026.01",
        "the flat view still answers the simple question: {p:#}"
    );
}

#[test]
fn with_nothing_claimed_the_answer_is_unknown_not_match() {
    // A false all-clear is worse than no check: it is the answer that stops
    // someone looking.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_with("2026.01"), None);
    let p = rig.call("provenance", json!({"device": device}));
    assert_eq!(p["verdict"], "unknown");
    assert!(
        p["why"].as_str().unwrap().contains("nothing has claimed"),
        "{}",
        p["why"]
    );
}

#[test]
fn a_silent_boot_cannot_be_checked_and_says_so() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("quiet.log", "[    1.0] nothing identifying here\n", None);
    bind_by_hand(&rig, &device, "2026.04");
    let p = rig.call("provenance", json!({"device": device}));
    assert_eq!(p["verdict"], "unknown");
    assert!(
        p["why"].as_str().unwrap().contains("no version banner"),
        "{}",
        p["why"]
    );
}

#[test]
fn a_matching_build_reads_as_a_match() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_with("2026.01"), None);
    bind_by_hand(&rig, &device, "2026.01");
    let p = rig.call("provenance", json!({"device": device}));
    assert_eq!(p["verdict"], "match", "{p:#}");
}

#[test]
fn a_stale_image_is_called_a_mismatch_and_says_what_it_invalidates() {
    // The whole point: you flashed 2026.04, the board is still running 2026.01,
    // and everything you are about to conclude is about the older image.
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_with("2026.01"), None);
    bind_by_hand(&rig, &device, "2026.04");
    let p = rig.call("provenance", json!({"device": device}));
    assert_eq!(p["verdict"], "mismatch", "{p:#}");
    let why = p["why"].as_str().unwrap();
    assert!(why.contains("2026.04"), "{why}");
    assert!(why.contains("2026.01"), "{why}");
    assert!(
        why.contains("concluded from this boot"),
        "the consequence is stated, not left to be inferred: {why}"
    );
    // ...and a hand binding is never described as a flash (§G5b).
    assert!(
        !why.contains("flash"),
        "set_image records an assertion, not a push of bytes: {why}"
    );
}

#[test]
fn provenance_is_scoped_to_an_epoch() {
    // Two boots, two different images: asking about the older one must not
    // report the newer one's banner.
    let rig = McpRig::new();
    let text = format!("{}{}", boot_with("2026.01"), boot_with("2026.04"));
    let (device, _) = rig.ingest("two.log", &text, None);
    let boots = rig.call("list_boots", json!({"device": device, "limit": 10}));
    let ids: Vec<i64> = boots["boots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_i64().unwrap())
        .collect();

    let newest = rig.call("provenance", json!({"device": device, "boot": ids[0]}));
    let oldest = rig.call(
        "provenance",
        json!({"device": device, "boot": ids[ids.len() - 1]}),
    );
    assert_ne!(
        newest["running"], oldest["running"],
        "each epoch reports its own build: {newest:#} vs {oldest:#}"
    );
}

#[test]
fn an_unknown_epoch_is_a_structured_error() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_with("2026.01"), None);
    let e = rig.err("provenance", json!({"device": device, "boot": 99_999}));
    assert_eq!(e["code"], "UNKNOWN_BOOT");
}

// ----------------------------------- a fingerprint is how a board names a build -

/// The Uno-Q's epoch, verbatim in shape: a bootloader chain the framer knows,
/// then an OS that announces itself with a build fingerprint no profile parses.
fn sirocco_boot(fp: &str) -> String {
    format!(
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         Sirocco version 0.1.0-sirocco-unoq-appsdk (mojo 1.1.0.dev2026081405) #1 SMP PREEMPT \
         aarch64 build={fp}\n\
         BUILD fp={fp}\n\
         APP admit\n"
    )
}

/// A BOARD ANNOUNCES A FINGERPRINT; A BINDING CARRIES A NAME.
///
/// Reported from the bench: `sirocco-unoq-appsdk-271e11b419aa852e` was bound,
/// the epoch printed `build=271e11b419aa852e` twice and the live console agreed
/// -- and provenance said `mismatch`, having compared the bound OS image against
/// the only strings it had parsed, which were firmware: chip, uefi, xbl.
///
/// Two bugs in one answer: the fingerprint the board actually printed was never
/// looked at, and firmware versions were treated as evidence about an OS image.
#[test]
fn a_build_fingerprint_printed_by_the_board_reads_as_a_match() {
    let rig = McpRig::new();
    let fp = "271e11b419aa852e";
    let (device, _) = rig.ingest("boot.log", &sirocco_boot(fp), None);
    bind_by_hand(&rig, &device, &format!("sirocco-unoq-appsdk-{fp}"));

    let p = rig.call("provenance", json!({"device": device}));
    assert_eq!(
        p["verdict"], "match",
        "the board printed the bound build's fingerprint: {p:#}"
    );
    assert!(
        p["why"].as_str().unwrap_or_default().contains(fp),
        "and the answer must show the fingerprint it matched on: {}",
        p["why"]
    );
}

/// A DIFFERENT fingerprint is still a mismatch, or the check above is just an
/// all-clear machine.
#[test]
fn a_different_build_fingerprint_is_still_a_mismatch() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &sirocco_boot("271e11b419aa852e"), None);
    bind_by_hand(&rig, &device, "sirocco-unoq-appsdk-ffffffffffffffff");
    let p = rig.call("provenance", json!({"device": device}));
    assert_eq!(p["verdict"], "mismatch", "{p:#}");
}

/// FIRMWARE IS NOT THE OPERATING SYSTEM.
///
/// An epoch that printed only chip/uefi/xbl banners has not said what OS is
/// running. Comparing those against a bound OS image cannot produce a mismatch,
/// only a false one -- "I cannot tell" is the third answer this suite exists to
/// protect.
#[test]
fn firmware_banners_alone_cannot_contradict_an_os_binding() {
    let rig = McpRig::new();
    // A firmware banner the framer really parses, and NOTHING from the OS --
    // no version, no fingerprint. An epoch with no parsed component at all takes
    // a different path ("no version banner"), so this fixture has to produce one.
    let (device, _) = rig.ingest(
        "boot.log",
        "NOTICE:  BL1: v2.11(release):v2.11\nsome unremarkable line\n",
        None,
    );
    bind_by_hand(&rig, &device, "sirocco-unoq-appsdk-271e11b419aa852e");
    let p = rig.call("provenance", json!({"device": device}));
    // NON-VACUITY: the firmware component must really have been parsed.
    assert!(
        !p["running_versions"].as_object().unwrap().is_empty(),
        "the fixture must parse a firmware component, or this tests the wrong path: {p:#}"
    );
    assert_ne!(
        p["verdict"], "mismatch",
        "firmware strings are not evidence about an OS image: {p:#}"
    );
    assert_eq!(
        p["verdict"], "unknown",
        "the honest answer is \"I cannot tell\": {p:#}"
    );
}

/// A BOARD WHOSE BANNER NO PROFILE KNOWS IS STILL NAMING ITS BUILD.
///
/// Found on hardware AFTER the first fingerprint fix shipped: the Uno-Q's own
/// epoch parsed to no component at all, so the answer never reached the
/// comparison and read "this epoch printed no version banner" while
/// `fp=2413879641ace37b` sat on the line above, matching the bound image.
#[test]
fn a_fingerprint_is_evidence_even_when_no_component_parses() {
    let rig = McpRig::new();
    let fp = "2413879641ace37b";
    // No bootloader chain, no recognised banner: just the board naming itself,
    // exactly as an RTOS `version` command answers.
    let (device, _) = rig.ingest(
        "boot.log",
        &format!("VERSION SIROCCO M4 mojo=1.1.0.dev2026081405 fp={fp}\nsirocco> \n"),
        None,
    );
    bind_by_hand(&rig, &device, &format!("sirocco-unoq-ramprobe-{fp}"));

    let p = rig.call("provenance", json!({"device": device}));
    // NON-VACUITY: this must really be the no-component path.
    assert!(
        p["running_versions"].as_object().unwrap().is_empty(),
        "fixture must parse no component, or it tests the other arm: {p:#}"
    );
    assert_eq!(
        p["verdict"], "match",
        "the board printed the bound build's fingerprint: {p:#}"
    );
}

/// ...and a fingerprint that does not match is a mismatch, not a shrug.
#[test]
fn a_printed_fingerprint_that_differs_is_a_mismatch_not_unknown() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", "VERSION SIROCCO M4 fp=2413879641ace37b\n", None);
    bind_by_hand(&rig, &device, "sirocco-unoq-ramprobe-ffffffffffff");
    let p = rig.call("provenance", json!({"device": device}));
    assert_eq!(p["verdict"], "mismatch", "{p:#}");
}

/// Report #24: the board printed the very commit that was bound, and the
/// verdict still said "mismatch".
///
/// `set_image` binds a `name` AND a `git_sha`; `intended_image()` collapses the
/// binding to one `ref` (the name wins), and the verdict compared only that.
/// So the kernel banner announcing `git f1fb57060680` -- the exact SHA bound --
/// was never evidence, and the epoch was declared to be running another image.
/// The whole binding is the claim, so every identity in it is evidence.
#[test]
fn a_bound_git_sha_the_board_prints_is_a_match_not_a_mismatch() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest(
        "boot.log",
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         [    3.000000] Linux version 6.12.9-g f1fb57060680 (build@lab) (gcc 14.2.0) #1 SMP\n\
         [    3.100000] sirocco: build=cffb5412ae57e7e7 git f1fb57060680\n",
        None,
    );
    rig.call("acquire", json!({"device": device}));
    // The name carries a DIFFERENT fingerprint from the one the board prints,
    // exactly as on the rig: matching on the name alone can only ever fail.
    rig.call(
        "set_image",
        json!({"device": device,
               "name": "sirocco-unoq-appsdk-f1632cec483a613c",
               "git_sha": "f1fb57060680"}),
    );

    let p = rig.call("provenance", json!({"device": device}));
    let (verdict, why) = (
        p["verdict"].as_str().unwrap_or(""),
        p["why"].as_str().unwrap_or(""),
    );
    assert_eq!(
        verdict, "match",
        "the board printed the bound git SHA f1fb57060680: {why} -- {p}"
    );
}

/// The same commit, written at two lengths. A binding carrying the full 40-hex
/// SHA and a banner printing the abbreviated 12 are the same build, and only a
/// prefix comparison can say so.
#[test]
fn an_abbreviated_git_sha_still_matches_the_full_one_that_was_bound() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest(
        "boot.log",
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         [    3.000000] Linux version 6.12.9 (build@lab) #1 SMP\n\
         [    3.100000] sirocco: build=f1fb57060680\n",
        None,
    );
    rig.call("acquire", json!({"device": device}));
    rig.call(
        "set_image",
        json!({"device": device,
               "name": "nightly",
               "git_sha": "f1fb57060680aa11bb22cc33dd44ee55ff667788"}),
    );

    let p = rig.call("provenance", json!({"device": device}));
    assert_eq!(
        p["verdict"].as_str().unwrap_or(""),
        "match",
        "abbreviated banner SHA vs full bound SHA: {}",
        p["why"].as_str().unwrap_or("")
    );
}

/// ...and the guard the fix must not weaken: a DIFFERENT commit is still a
/// mismatch. Prefix matching that says "close enough" is a false all-clear.
#[test]
fn a_different_git_sha_is_still_a_mismatch() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest(
        "boot.log",
        "NOTICE:  BL1: v2.11(release):v2.11\n\
         [    3.000000] Linux version 6.12.9 (build@lab) #1 SMP\n\
         [    3.100000] sirocco: build=deadbeefcafe1234\n",
        None,
    );
    rig.call("acquire", json!({"device": device}));
    rig.call(
        "set_image",
        json!({"device": device, "name": "nightly", "git_sha": "f1fb57060680"}),
    );

    let p = rig.call("provenance", json!({"device": device}));
    assert_eq!(
        p["verdict"].as_str().unwrap_or(""),
        "mismatch",
        "a different commit must stay a mismatch: {}",
        p["why"].as_str().unwrap_or("")
    );
}

/// Reports #29/#30: `boot_report` died with "INTERNAL: Conversion error from
/// type Text ... invalid utf-8 sequence" on an epoch whose console bytes were
/// not valid UTF-8.
///
/// A board mid-reset emits NULs and half-formed UTF-8; the fingerprint scan
/// asked rusqlite for a `String` and the whole call failed. A corrupted byte
/// must cost that byte, never the report.
#[test]
fn a_corrupt_console_byte_cannot_take_down_boot_report() {
    let rig = McpRig::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.log");
    let mut bytes: Vec<u8> = Vec::new();
    // #30's epoch began exactly like this: a truncated UEFI banner and a NUL.
    bytes.extend_from_slice(b"UEF\xff\x00 Ver : 6.0.260212.BOOT\n");
    bytes.extend_from_slice(b"[    3.000000] Linux version 6.12.9 (build@lab) #1 SMP\n");
    bytes.extend_from_slice(b"sirocco: build=cffb5412ae57e7e7 git f1fb57060680\n");
    std::fs::write(&path, &bytes).unwrap();

    let ing = rig.call("ingest_file", json!({"path": path.display().to_string()}));
    let device = ing["device"].as_str().expect("device").to_string();

    let br = rig.call("boot_report", json!({"device": &device}));
    assert!(
        br.get("error").map_or(true, |e| e.is_null()),
        "boot_report must survive non-UTF-8 console bytes: {br}"
    );
    // ...and the readable part of the epoch is still mined, not discarded.
    let fps = br["build_fingerprints"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        fps.iter().any(|f| f.as_str() == Some("cffb5412ae57e7e7")),
        "the printed fingerprint must survive the corrupt line: {br}"
    );
}

/// The row limit is a budget, and a bare-substring match spends it on noise.
///
/// Matching `%build%`/`%sha%` anywhere pulled in most of a kernel log, so the
/// 200-row cap filled with ordinary lines and the one line carrying the
/// fingerprint never got read.
#[test]
fn ordinary_log_noise_cannot_crowd_out_the_line_that_names_the_build() {
    let rig = McpRig::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("noisy.log");
    let mut log = String::new();
    for i in 0..400 {
        // Words that contain the identity keys as substrings, as a real kernel
        // log does: "reverting", "shall", "rebuild", "gitless".
        log.push_str(&format!(
            "[{i:>9}.000000] mmc0: reverting shall rebuild gitless block {i}\n"
        ));
    }
    log.push_str("sirocco: build=ad945cfe883cc603\n");
    std::fs::write(&path, &log).unwrap();

    let ing = rig.call("ingest_file", json!({"path": path.display().to_string()}));
    let device = ing["device"].as_str().expect("device").to_string();

    let br = rig.call("boot_report", json!({"device": &device}));
    let fps = br["build_fingerprints"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        fps.iter().any(|f| f.as_str() == Some("ad945cfe883cc603")),
        "the build line must be found behind 400 noise lines: {br}"
    );
}

/// Report #27, structurally: A HINT THAT NAMES A TOOL MUST NAME A REAL ONE.
///
/// `ExclusiveClaimed` hinted `release_exclusive(device)`, which has never
/// appeared in `tools/list`. An agent whose claim holder had died read the
/// hint, went looking, found nothing, and filed a report saying the state was
/// unrecoverable -- while `claim_exclusive(device, release: true)` had been
/// there the whole time. Every hint that reads like a call is checked here, so
/// the next one cannot invent a tool either.
#[test]
fn no_error_hint_may_name_a_tool_that_does_not_exist() {
    let rig = McpRig::new();
    let listed = rig.call("help", json!({}));
    let names: std::collections::BTreeSet<String> = listed["tools"]
        .as_array()
        .map(|ts| {
            ts.iter()
                .filter_map(|t| {
                    t.get("name")
                        .and_then(|n| n.as_str())
                        .or_else(|| t.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !names.is_empty(),
        "no tool names to check against: {listed}"
    );

    // Words that read like `something(` inside a hint are claims about the
    // surface, so each one has to be on it.
    let mut bad = Vec::new();
    for code in conminer_core::ErrorCode::all() {
        let hint = code.default_hint();
        let bytes = hint.as_bytes();
        for (i, _) in hint.match_indices('(') {
            let start = bytes[..i]
                .iter()
                .rposition(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
                .map_or(0, |p| p + 1);
            let word = &hint[start..i];
            if word.len() < 4 || !word.contains('_') {
                continue; // prose like "(see ...)", never a tool name
            }
            if !names.contains(word) {
                bad.push(format!("{code:?} hints {word}(), which is not a tool"));
            }
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

/// Report #27: a stale exclusive claim could not be cleared by anything.
///
/// `claim_exclusive` hands one console to one protocol, and the holder that
/// took it can die mid-transfer. Its claim then refuses every later caller
/// while the hint points at a `release_exclusive(device)` tool that has never
/// existed in `tools/list`. Dropping the lease is the moment the claim stopped
/// meaning anything, so it goes with it.
#[test]
fn releasing_the_lease_clears_a_claim_that_outlived_its_holder() {
    let rig = McpRig::new();
    let (device, _) = rig.ingest("boot.log", &boot_with("2026.01"), None);

    rig.call("acquire", json!({"device": &device}));
    let claimed = rig.call(
        "claim_exclusive",
        json!({"device": &device, "protocol": "zmodem"}),
    );
    assert!(
        claimed.get("error").map_or(true, |e| e.is_null()),
        "claim_exclusive should succeed under our own lease: {claimed}"
    );

    // The holder goes away, the way a timed-out client does.
    rig.call("release", json!({"device": &device}));

    // A later caller must be able to work with this console again.
    rig.call("acquire", json!({"device": &device}));
    let again = rig.call(
        "claim_exclusive",
        json!({"device": &device, "protocol": "zmodem"}),
    );
    assert_ne!(
        again["error"]["code"], "EXCLUSIVE_CLAIMED",
        "a claim must not outlive the lease it was taken under: {again}"
    );
}
