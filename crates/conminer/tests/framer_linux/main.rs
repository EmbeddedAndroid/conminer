//! Suite `framer-linux` (§13) — the `linux` profile.
//!
//! Edge cases from the catalog:
//!   single printk · nested oops in irq context · `Call Trace:` continuation ·
//!   truncated panic (power loss mid-record → record closed with `truncated`) ·
//!   interleaved SMP lines (flagged per §11) · OOM report · lockdep splat ·
//!   loglevel-less lines · printk time going backwards after RTC sync
//!
//! Plus the §A.9 superset guarantee: anything LAVA's `LinuxKernelMessages` or
//! SQUAD's `linux_log_parser` calls a failure, this profile must call at least
//! as severe.

use conminer_core::store::{RecordKind, Severity};
use conminer_testkit::corpus::corpus_text;
use conminer_testkit::frame::{frame_text, frame_then_idle};

#[test]
fn a_single_printk_is_a_single_record() {
    let r = frame_text(
        "linux",
        "[    1.102345] mmc0: new HS200 MMC card at address 0001\n",
    );
    assert_eq!(r.records.len(), 1);
    assert_eq!(r.records[0].line_count, 1);
    assert_eq!(r.records[0].kind, RecordKind::Line);
    r.assert_conserved();
}

#[test]
fn the_printk_timestamp_is_extracted_not_removed() {
    let r = frame_text("linux", "[    1.102345] mmc0: new HS200 MMC card\n");
    let rec = &r.records[0];
    assert_eq!(rec.fields["ts_printk"], "1.102345");
    // The record's text keeps every byte…
    assert!(rec.text.starts_with("[    1.102345]"));
    // …while the mining key drops the per-boot prefix, or every line would be
    // its own template (§6, §14.4).
    assert_eq!(rec.mine_key.as_deref(), Some("mmc0: new HS200 MMC card"));
}

#[test]
fn an_oops_is_one_record_with_its_whole_dump() {
    let r = frame_text("linux", &corpus_text("linux/boot-oops.log"));
    r.assert_conserved();

    let crashes = r.crashes();
    assert_eq!(crashes.len(), 1, "one oops, one crash record");
    let oops = crashes[0];
    assert_eq!(oops.severity, Severity::Emerg);

    let lines = r.record_lines(oops);
    assert!(lines[0].contains("Internal error: Oops"));
    assert!(lines.iter().any(|l| l.contains("Modules linked in:")));
    assert!(lines.iter().any(|l| l.contains("Call trace:")));
    assert!(lines.iter().any(|l| l.contains("really_probe")));
    assert!(lines.last().unwrap().contains("end trace"));
    assert_eq!(oops.fields["closed_by"], "terminator");
}

#[test]
fn call_trace_frames_continue_the_record() {
    let r = frame_text(
        "linux",
        "[ 1.0] Internal error: Oops: 96000006 [#1] PREEMPT SMP\n\
         [ 1.0] Call trace:\n\
         [ 1.0]  really_probe+0xc8/0x3a0\n\
         [ 1.0]  driver_probe_device+0x40/0x110\n\
         [ 1.0] ---[ end trace 0000000000000000 ]---\n\
         [ 2.0] mmc0: card is fine\n",
    );
    r.assert_conserved();
    assert_eq!(r.crashes().len(), 1);
    assert_eq!(r.crashes()[0].line_count, 5);
    // The unrelated line after the terminator is its own record.
    assert_eq!(r.records.last().unwrap().line_count, 1);
}

#[test]
fn a_nested_trigger_closes_the_outer_record_rather_than_swallowing_it() {
    // Panic inside panic: two events, honestly reported as two.
    let r = frame_text(
        "linux",
        "[ 1.0] Internal error: Oops: 0 [#1] PREEMPT SMP\n\
         [ 1.0] Modules linked in: foo\n\
         [ 1.1] Kernel panic - not syncing: Fatal exception in interrupt\n\
         [ 1.1] CPU: 0 PID: 1 Comm: swapper\n",
    );
    r.assert_conserved();
    assert_eq!(r.crashes().len(), 2);
    assert_eq!(r.crashes()[0].fields["closed_by"], "nested_trigger");
}

#[test]
fn a_panic_cut_by_power_loss_closes_with_the_truncated_flag() {
    let text = corpus_text("hostile/truncated-oops.log");
    let r = frame_text("linux", &text);
    r.assert_conserved();
    let last = r.records.last().unwrap();
    assert!(last.truncated, "a record cut by end-of-stream must say so");
    assert_eq!(last.fields["closed_by"], "stream_end");
}

#[test]
fn a_record_that_goes_silent_closes_on_dead_air() {
    let r = frame_then_idle(
        "linux",
        "[ 1.0] Internal error: Oops: 0 [#1] PREEMPT SMP\n\
         [ 1.0] Modules linked in: foo\n",
        60_000,
    );
    assert_eq!(r.crashes().len(), 1);
    assert_eq!(r.crashes()[0].fields["closed_by"], "dead_air");
}

#[test]
fn interleaved_smp_lines_are_flagged_not_silently_reassembled() {
    // §11: v1 frames greedily and flags the suspicion. Claiming to have
    // reassembled interleaved output would be a lie an agent would act on.
    let r = frame_text(
        "linux",
        "[ 1.0] Internal error: Oops: 0 [#1] PREEMPT SMP\n\
         [ 1.0] Call trace:\n\
         [ 1.0]  really_probe+0xc8/0x3a0\n\
         [ 1.0] CPU: 3 PID: 9 Comm: kworker/3:0\n\
         [ 1.0]  driver_probe_device+0x40/0x110\n\
         [ 1.0] ---[ end trace 0 ]---\n",
    );
    let oops = r.crashes()[0];
    assert_eq!(oops.fields["interleave_suspected"], true);
}

#[test]
fn an_oom_report_is_a_record() {
    let r = frame_text(
        "linux",
        "[ 9.0] Out of memory: Killed process 412 (node) total-vm:1024kB\n\
         [ 9.0] Mem-Info:\n\
         [ 9.0] active_anon:1234 inactive_anon:0 isolated_anon:0\n\
         [ 9.1] EXT4-fs (sda1): mounted filesystem\n",
    );
    r.assert_conserved();
    let oom = r.record_containing("Out of memory");
    assert_eq!(oom.line_count, 3);
    assert_eq!(oom.severity, Severity::Err);
    assert_eq!(oom.kind, RecordKind::Line, "an OOM kill is not a panic");
}

#[test]
fn a_lockdep_splat_is_a_record() {
    let r = frame_text(
        "linux",
        "[ 5.0] WARNING: possible circular locking dependency detected\n\
         [ 5.0] 6.12.9 #1 Not tainted\n\
         [ 5.0] other info that might help us debug this:\n\
         [ 5.0] possible unsafe locking scenario:\n\
         [ 5.1] usb 1-1: new high-speed USB device\n",
    );
    r.assert_conserved();
    let splat = r.record_containing("circular locking");
    assert!(splat.line_count >= 3);
    assert_eq!(splat.severity, Severity::Warn);
}

#[test]
fn a_cut_here_warning_pairs_with_its_end_trace() {
    let r = frame_text(
        "linux",
        "[ 2.0] ------------[ cut here ]------------\n\
         [ 2.0] WARNING: CPU: 1 PID: 33 at drivers/clk/clk.c:1099 clk_core_disable+0x1a4\n\
         [ 2.0] Modules linked in: gpucc\n\
         [ 2.0] Call trace:\n\
         [ 2.0]  clk_core_disable+0x1a4/0x1c0\n\
         [ 2.0] ---[ end trace 0000000000000000 ]---\n",
    );
    r.assert_conserved();
    assert_eq!(r.records.len(), 1, "a cut-here block is one event");
    assert_eq!(r.records[0].line_count, 6);
}

#[test]
fn loglevel_less_lines_still_classify() {
    let r = frame_text("linux", "mmc0: error -110 whilst initialising SD card\n");
    assert_eq!(r.records[0].severity, Severity::Err);
    assert!(r.records[0].fields.get("ts_printk").is_none());
}

#[test]
fn raw_printk_loglevels_win_over_the_generic_heuristic() {
    let r = frame_text("linux", "<4>mmc0: error -110 whilst initialising\n");
    assert_eq!(
        r.records[0].severity,
        Severity::Warn,
        "an explicit <4> outranks the word 'error'"
    );
    assert_eq!(r.records[0].fields["loglevel"], "4");
}

#[test]
fn printk_time_going_backwards_after_rtc_sync_changes_nothing() {
    // §14.4: target-side time is an extracted field, never trusted for ordering.
    // Framing must be identical whether or not the clock steps.
    let forwards = "[ 1.000000] a one\n[ 2.000000] a two\n[ 3.000000] a three\n";
    let backwards = "[ 1.000000] a one\n[ 9999.000000] a two\n[ 3.000000] a three\n";
    let a = frame_text("linux", forwards);
    let b = frame_text("linux", backwards);
    assert_eq!(a.records.len(), b.records.len());
    for (x, y) in a.records.iter().zip(&b.records) {
        assert_eq!(x.first_line_id, y.first_line_id);
        assert_eq!(x.mine_key, y.mine_key, "the key must not carry the stamp");
    }
}

// -------------------------------------------- §A.9 superset guarantee --------

/// Every pattern LAVA's `LinuxKernelMessages` and SQUAD's `linux_log_parser`
/// treat as a failure. conminer must classify each at least as severely — never
/// less (§A.9). Ported patterns, not code (both upstreams are GPL-2.0+).
const LAVA_SQUAD_FAILURES: &[&str] = &[
    "Kernel panic - not syncing: Attempted to kill init!",
    "Kernel panic - not syncing: VFS: Unable to mount root fs on unknown-block(0,0)",
    "Kernel panic - not syncing: Fatal exception",
    "Kernel panic - not syncing: Fatal exception in interrupt",
    "Internal error: Oops: 96000006 [#1] PREEMPT SMP",
    "Oops: 0000 [#1] SMP PTI",
    "kernel BUG at mm/slub.c:389!",
    "BUG: unable to handle kernel NULL pointer dereference at 0000000000000018",
    "BUG: unable to handle page fault for address: ffffffffc0000000",
    "general protection fault: 0000 [#1] SMP",
    "invalid opcode: 0000 [#1] SMP",
    "Unhandled fault: alignment fault (0x96000021) at 0x0000000000000001",
    "Unable to handle kernel paging request at virtual address 00000000ffffffff",
    "------------[ cut here ]------------",
    "WARNING: CPU: 0 PID: 1 at kernel/sched/core.c:3117 ttwu_queue_wakelist+0x1c",
    "watchdog: BUG: soft lockup - CPU#0 stuck for 22s! [kworker/0:1:33]",
    "INFO: task kworker/0:1:33 blocked for more than 120 seconds.",
    "rcu: INFO: rcu_sched detected stalls on CPUs/tasks:",
    "BUG: scheduling while atomic: swapper/0/0/0x00000002",
    "BUG: sleeping function called from invalid context at mm/page_alloc.c:5116",
    "Bad mode in Synchronous Abort handler detected on CPU0, code 0xbf000002",
    "SError Interrupt on CPU0, code 0xbe000411",
    "stack-protector: Kernel stack is corrupted in: schedule+0x1a4/0x1c0",
];

#[test]
fn superset_guarantee_over_the_lava_and_squad_pattern_sets() {
    for line in LAVA_SQUAD_FAILURES {
        let r = frame_text("linux", &format!("[    1.234567] {line}\n"));
        let rec = &r.records[0];
        assert!(
            rec.severity <= Severity::Warn,
            "conminer must classify {line:?} at least as severely as LAVA/SQUAD do, \
             got {:?}",
            rec.severity
        );
    }
}

#[test]
fn the_fatal_half_of_that_set_opens_crash_records() {
    let fatal = &LAVA_SQUAD_FAILURES[..13];
    for line in fatal {
        let r = frame_text("linux", &format!("[    1.234567] {line}\n"));
        assert!(
            r.records[0].severity <= Severity::Crit,
            "{line:?} should be crit-or-worse, got {:?}",
            r.records[0].severity
        );
    }
}

#[test]
fn userspace_runtime_crashes_are_caught_too() {
    // §A.11.6: on embedded Linux the crash that matters is increasingly a
    // language-runtime panic on the userspace stage.
    for line in [
        "thread 'main' panicked at src/main.rs:42:5:",
        "panic: runtime error: index out of range [3] with length 2",
        "Traceback (most recent call last):",
        "terminate called after throwing an instance of 'std::runtime_error'",
        "*** stack smashing detected ***: terminated",
        "double free or corruption (out)",
    ] {
        let r = frame_text("linux", &format!("{line}\n"));
        assert!(
            r.records[0].severity <= Severity::Err,
            "{line:?} → {:?}",
            r.records[0].severity
        );
    }
}

/// Interleaved console output must not give one kernel message eight identities.
///
/// Consoles interleave: systemd writes a status line with NO newline and a
/// kernel printk lands in the middle of it. Measured on a real boot, this is
/// what "read descriptors" actually looked like in the template table --
/// eight rows for one message:
///     "Starting DNS forwarder and DHCP server... [ 16.470867] read descriptors"
///     "ventuno-q-535812139 login: [ 17.1] read descriptors"
///     "+q6E616D65[ 18.2] read descriptors"
/// The miner was right; the input was dirty. Mining resyncs at the printk
/// timestamp, and the raw line keeps every byte the board sent.
#[test]
fn a_printk_interleaved_into_another_line_still_clusters_with_itself() {
    let profiles = conminer_core::framer::ProfileSet::builtin().unwrap();
    let linux = profiles.get("linux").expect("linux profile");

    let clean = linux.mine_key("[   16.470867] read descriptors");
    for dirty in [
        "Starting DNS forwarder and DHCP server... [   16.470867] read descriptors",
        "ventuno-q-535812139 login: [   16.470867] read descriptors",
        "+q6E616D65[   16.470867] read descriptors",
        "\u{feff}garbage prefix [   16.470867] read descriptors",
    ] {
        assert_eq!(
            linux.mine_key(dirty),
            clean,
            "an interleaved prefix must not create a second identity for:\n  {dirty}"
        );
    }
    assert_eq!(clean, "read descriptors");
}

/// Resync must not eat a line that legitimately BEGINS with its timestamp --
/// the overwhelmingly common case.
#[test]
fn an_ordinary_kernel_line_is_unaffected_by_resync() {
    let profiles = conminer_core::framer::ProfileSet::builtin().unwrap();
    let linux = profiles.get("linux").expect("linux profile");
    assert_eq!(
        linux.mine_key("[    3.000000] Linux version 6.12.9 (build@lab)"),
        "Linux version 6.12.9 (build@lab)"
    );
    // And a line with no timestamp at all is untouched apart from trimming.
    assert_eq!(
        linux.mine_key("Freeing unused kernel memory"),
        "Freeing unused kernel memory"
    );
}

/// Only the DERIVED key is trimmed -- `mine_key` never touches its input, and
/// raw conservation is guaranteed separately by the line splitter's fuzz
/// invariant. Stated here because a resync that quietly deleted the prefix from
/// storage would be deleting evidence rather than clarifying it.
#[test]
fn resync_trims_only_the_derived_key() {
    let profiles = conminer_core::framer::ProfileSet::builtin().unwrap();
    let linux = profiles.get("linux").expect("linux profile");
    let dirty = "Starting DNS forwarder... [   16.470867] read descriptors";
    let key = linux.mine_key(dirty);
    assert_eq!(key, "read descriptors");
    assert!(
        dirty.contains("Starting DNS forwarder"),
        "the caller's line is untouched; only the derived view is trimmed"
    );
}
