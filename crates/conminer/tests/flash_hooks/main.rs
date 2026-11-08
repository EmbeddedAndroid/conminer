//! Suite `flash_hooks`: the generic EDL flash hook (`hooks/qdl-flash`).
//!
//! Each test stages a board the way a node does (stage directory, serial file,
//! qdl runtime, firehose set, board file) and runs the real script. The qdl
//! runtime is a stand-in loader that records every invocation and emulates
//! reads, writes and on-device digests, so the assertions are on the exact argv
//! the hook would hand qdl and on what it refuses before qdl is ever reached.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SERIAL: &str = "ABCD1234";

fn hook() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../hooks/qdl-flash"
    ))
}

fn manifests_dir() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../hooks/manifests"
    ))
}

/// The stand-in for `ld-linux-x86-64.so.2 --library-path <dir> <qdl|fastboot>`.
const FAKE_LOADER: &str = r#"#!/bin/sh
rt=$(cd "$(dirname "$0")" && pwd)
st=$(dirname "$rt")
[ "$1" = --library-path ] || exit 99
shift 2
prog=$(basename "$1")
shift
{
	echo "CALL $prog"
	echo "PWD $(pwd)"
	if [ -e /proc/self/fd/9 ]; then echo "FD9 open"; fi
	for a in "$@"; do echo "ARG $a"; done
	echo "END"
} >> "$st/calls.log"
[ ! -f "$st/fake-exit" ] || exit "$(cat "$st/fake-exit")"
dry=0
for a in "$@"; do [ "$a" != --dry-run ] || dry=1; done
mkdir -p "$st/store"
key() { printf '%s' "$1" | tr '/+' '__'; }
while [ $# -gt 0 ]; do
	case "$1" in
	write)
		if [ $dry = 0 ]; then cp "$3" "$st/store/$(key "$2")"; cp "$3" "$st/store/last"; fi
		shift 3 ;;
	read)
		if [ $dry = 0 ]; then
			if [ -f "$st/store/$(key "$2")" ]; then
				cp "$st/store/$(key "$2")" "$3"
			else
				n=${2##*+}
				case "$2" in *+*) ;; *) n=1 ;; esac
				[ ! -f "$st/short-read" ] || n=$((n - 1))
				head -c $((n * 4096)) /dev/zero > "$3"
			fi
			[ ! -f "$st/corrupt-readback" ] || printf x >> "$3"
		fi
		shift 3 ;;
	sha256)
		if [ -f "$st/fake-sha" ]; then cat "$st/fake-sha"
		else sha256sum "$st/store/last" | cut -d' ' -f1; fi
		shift 2 ;;
	*) shift ;;
	esac
done
"#;

struct Call {
    prog: String,
    pwd: String,
    fd9_open: bool,
    args: Vec<String>,
}

struct Rig {
    _tmp: tempfile::TempDir,
    stage: PathBuf,
    board: PathBuf,
}

fn sha256(path: &Path) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(std::fs::read(path).unwrap()))
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

fn exec(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    write(path, text);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A manifest over `files` (relative to `dir`), written outside the stage.
fn manifest(dir: &Path, files: &[&str], out: &Path) {
    let text: String = files
        .iter()
        .map(|f| format!("{}  {}\n", sha256(&dir.join(f)), f))
        .collect();
    write(out, text);
}

/// `SHA256SUMS` in `dir` over `files`.
fn sums(dir: &Path, name: &str, files: &[&str]) {
    let text: String = files
        .iter()
        .map(|f| format!("{}  {}\n", sha256(&dir.join(f)), f))
        .collect();
    write(&dir.join(name), text);
}

impl Rig {
    /// A stage with a qdl runtime, a firehose set holding `loaders`, and the
    /// serial file. `board` is the board file minus its stage/runtime lines.
    fn new(loaders: &[&str], board: &str) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("stage");
        let rt = stage.join("runtime");
        exec(&rt.join("ld-linux-x86-64.so.2"), FAKE_LOADER);
        write(&rt.join("qdl"), "qdl stand-in");
        write(&rt.join("libusb-1.0.so.0"), "lib stand-in");
        let fh = stage.join("firehose");
        for l in loaders {
            write(&fh.join(l), format!("loader {l}"));
        }
        write(&stage.join("edl-serial"), format!("{SERIAL}\n"));
        let m = tmp.path().join("manifests");
        manifest(
            &rt,
            &["ld-linux-x86-64.so.2", "libusb-1.0.so.0", "qdl"],
            &m.join("runtime.sha256"),
        );
        manifest(&fh, loaders, &m.join("firehose.sha256"));
        let board_path = tmp.path().join("board");
        let rig = Rig {
            stage,
            board: board_path,
            _tmp: tmp,
        };
        rig.set_board(board);
        rig
    }

    fn manifest_dir(&self) -> PathBuf {
        self.stage.parent().unwrap().join("manifests")
    }

    fn set_board(&self, extra: &str) {
        let m = self.manifest_dir();
        let text = format!(
            "stage {}\nruntime runtime {}\nfirehose firehose {}\n{extra}\n",
            self.stage.display(),
            m.join("runtime.sha256").display(),
            m.join("firehose.sha256").display(),
        );
        write(&self.board, text);
    }

    fn dir(&self, name: &str) -> PathBuf {
        let d = self.stage.join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Runs the hook; `calls` then describes this run only.
    fn run(&self, args: &[&str]) -> Output {
        let _ = std::fs::remove_file(self.stage.join("calls.log"));
        Command::new("bash")
            .arg(hook())
            .arg("--board")
            .arg(&self.board)
            .args(args)
            .output()
            .expect("hook runs")
    }

    fn calls(&self) -> Vec<Call> {
        let text = std::fs::read_to_string(self.stage.join("calls.log")).unwrap_or_default();
        let mut out = Vec::new();
        let mut cur: Option<Call> = None;
        for line in text.lines() {
            if let Some(p) = line.strip_prefix("CALL ") {
                cur = Some(Call {
                    prog: p.into(),
                    pwd: String::new(),
                    fd9_open: false,
                    args: Vec::new(),
                });
            } else if let Some(c) = cur.as_mut() {
                if let Some(p) = line.strip_prefix("PWD ") {
                    c.pwd = p.into();
                } else if line == "FD9 open" {
                    c.fd9_open = true;
                } else if let Some(a) = line.strip_prefix("ARG ") {
                    c.args.push(a.into());
                } else if line == "END" {
                    out.push(cur.take().unwrap());
                }
            }
        }
        out
    }
}

fn text(o: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn ok(o: &Output) {
    assert!(o.status.success(), "hook failed:\n{}", text(o));
}

/// Everything the hook printed. Once its log is open, stderr joins stdout.
fn said(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// Refused before qdl: exit 2, the reason given, and no qdl call at all.
fn refused(rig: &Rig, o: &Output, why: &str) {
    assert_eq!(o.status.code(), Some(2), "expected a refusal:\n{}", text(o));
    assert!(
        said(o).contains(why),
        "refusal does not say {why:?}:\n{}",
        text(o)
    );
    assert!(
        rig.calls().is_empty(),
        "qdl ran although the hook refused:\n{}",
        text(o)
    );
}

/// The qdl operations after the programmer argument.
fn ops(c: &Call) -> Vec<String> {
    let at = c
        .args
        .iter()
        .position(|a| !a.starts_with("--"))
        .expect("programmer argument");
    c.args[at + 1..].to_vec()
}

fn programmer(c: &Call) -> String {
    c.args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap()
}

// ------------------------------------------------------------- boards ------

const SAHARA: &[&str] = &["qsahara_device_programmer.xml", "xbl_sc.elf"];

/// The A/B-on-two-LUNs layout (B slot on LUN 5).
const SPLIT_LUN_BOARD: &str = "\
storage ufs
programmer qsahara_device_programmer.xml
reset no
debug yes
ref kind FP-[0-9]{6}-[0-9]{6}|PAIR-[a-f0-9]{12}
ref xml STOCK-[A-Za-z0-9._-]+ own-programmer
ref xml IMG-[A-Za-z0-9._-]+
ref read READ-[A-Za-z0-9._-]+
kind fip fip.elf 4/uefi_a 5/uefi_b
kind tz tz.mbn 4/tz_a 5/tz_b
kind boot-chain tz.mbn 4/tz_a 5/tz_b
kind boot-chain fip.elf 4/uefi_a 5/uefi_b
kind efi efi.bin 0/efi
kind apdp apdp.mbn 1/apdp_a 2/apdp_b
kind uefi-dtb uefi_dtbs.elf 4/uefi_dtb_a 5/uefi_dtb_b
max_bytes apdp.mbn 65536
";

/// Same SoC, both slots on LUN 4.
const ONE_LUN_BOARD: &str = "\
storage ufs
programmer qsahara_device_programmer.xml
reset no
debug yes
ref kind FP-[0-9]{6}-[0-9]{6}|PAIR-[a-f0-9]{12}
kind fip fip.elf 4/uefi_a 4/uefi_b
kind tz tz.mbn 4/tz_a 4/tz_b
kind boot-chain tz.mbn 4/tz_a 4/tz_b
kind boot-chain fip.elf 4/uefi_a 4/uefi_b
kind efi efi.bin 0/efi
kind apdp apdp.mbn 4/apdp_a 4/apdp_b
kind uefi-dtb uefi_dtbs.elf 4/uefi_dtb_a 4/uefi_dtb_b
max_bytes apdp.mbn 65536
";

const RESET_BOARD: &str = "\
storage ufs
programmer prog_firehose_ddr.elf prog_firehose*.elf prog_*.elf *devprg*.melf
reset yes
fastboot_serial_file fastboot-serial
ref xml STOCK-[A-Za-z0-9._-]+ firehose-programmer
ref xml-or-parts FP-[0-9]{6}-[0-9A-Za-z-]+ firehose-programmer
ref fastboot-boot BOOT-[A-Za-z0-9._-]+
ref read READ-[A-Za-z0-9._-]+ own-programmer
";

const NOR_BOARD: &str = "\
storage spinor
sector_bytes 4096
programmer prog_firehose_ddr.elf
reset yes
nor_payload sail_hyp.bin
ref window-write SAILZ-[0-9]{1,6} firehose-programmer
ref window-read NORREAD-[A-Za-z0-9._-]{1,64} firehose-programmer
window PRIMARY 0/7+4 rw
window BACKUP 0/1781+4 rw
window FULL 0/0+16 r
";

const CONSOLE: &str = "/dev/serial/by-id/usb-Example_Board_X1-if00-port0";

fn digest_board() -> String {
    format!(
        "storage emmc
sector_bytes 512
programmer prog_firehose_ddr.elf
reset no
timeout_s 240
console {CONSOLE}
digest_dir work
ref digest v1:[a-z0-9_]+:[0-9a-f]{{64}}:[A-Za-z0-9_.-]+
digest efi 0/efi whole exact 4096
digest rootfs 0/rootfs 0/2306184 max 8192
"
    )
}

/// A kind directory: payloads, SHA256SUMS.flash in payload order, flash-kind.
fn stage_kind(rig: &Rig, name: &str, kind: Option<&str>, payloads: &[(&str, usize)]) -> PathBuf {
    let d = rig.dir(name);
    for (f, n) in payloads {
        write(&d.join(f), vec![b'p'; *n]);
    }
    let files: Vec<&str> = payloads.iter().map(|(f, _)| *f).collect();
    sums(&d, "SHA256SUMS.flash", &files);
    if let Some(k) = kind {
        write(&d.join("flash-kind.txt"), format!("{k}\n"));
    }
    d
}

fn writes(c: &Call) -> Vec<(String, String)> {
    let o = ops(c);
    o.chunks(3)
        .map(|w| {
            assert_eq!(w[0], "write", "expected only writes: {o:?}");
            (
                w[1].clone(),
                Path::new(&w[2])
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into(),
            )
        })
        .collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

// --------------------------------------------------------------- kind ------

#[test]
fn a_kind_directory_without_flash_kind_writes_the_first_kind() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    let d = stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 100)]);
    let o = rig.run(&["FP-260921-222651"]);
    ok(&o);
    let calls = rig.calls();
    assert_eq!(calls.len(), 1, "one qdl session for every write");
    let c = &calls[0];
    assert_eq!(c.prog, "qdl");
    assert_eq!(
        writes(c),
        pairs(&[("4/uefi_a", "fip.elf"), ("5/uefi_b", "fip.elf")])
    );
    for flag in [
        "--storage=ufs",
        &format!("--serial={SERIAL}"),
        "--skip-reset",
        "--debug",
    ] {
        assert!(
            c.args.iter().any(|a| a == flag),
            "missing {flag}: {:?}",
            c.args
        );
    }
    assert!(!c.args.iter().any(|a| a == "--dry-run"));
    // A Sahara XML names its loaders relative to the working directory.
    assert_eq!(Path::new(&c.pwd), rig.stage.join("firehose"));
    assert_eq!(
        Path::new(&programmer(c)),
        rig.stage.join("firehose/qsahara_device_programmer.xml")
    );
    assert!(
        d.join("flash.log").exists(),
        "the run is logged beside the image"
    );
}

/// A kind, its payloads, and its writes on the split-LUN and one-LUN boards.
type KindCase<'a> = (
    &'a str,
    &'a [(&'a str, usize)],
    &'a [(&'a str, &'a str)],
    &'a [(&'a str, &'a str)],
);

#[test]
fn every_kind_maps_to_the_targets_of_its_board() {
    let cases: &[KindCase] = &[
        (
            "tz",
            &[("tz.mbn", 10)],
            &[("4/tz_a", "tz.mbn"), ("5/tz_b", "tz.mbn")],
            &[("4/tz_a", "tz.mbn"), ("4/tz_b", "tz.mbn")],
        ),
        (
            "boot-chain",
            &[("tz.mbn", 10), ("fip.elf", 10)],
            &[
                ("4/tz_a", "tz.mbn"),
                ("5/tz_b", "tz.mbn"),
                ("4/uefi_a", "fip.elf"),
                ("5/uefi_b", "fip.elf"),
            ],
            &[
                ("4/tz_a", "tz.mbn"),
                ("4/tz_b", "tz.mbn"),
                ("4/uefi_a", "fip.elf"),
                ("4/uefi_b", "fip.elf"),
            ],
        ),
        (
            "efi",
            &[("efi.bin", 10)],
            &[("0/efi", "efi.bin")],
            &[("0/efi", "efi.bin")],
        ),
        (
            "apdp",
            &[("apdp.mbn", 10)],
            &[("1/apdp_a", "apdp.mbn"), ("2/apdp_b", "apdp.mbn")],
            &[("4/apdp_a", "apdp.mbn"), ("4/apdp_b", "apdp.mbn")],
        ),
        (
            "uefi-dtb",
            &[("uefi_dtbs.elf", 10)],
            &[
                ("4/uefi_dtb_a", "uefi_dtbs.elf"),
                ("5/uefi_dtb_b", "uefi_dtbs.elf"),
            ],
            &[
                ("4/uefi_dtb_a", "uefi_dtbs.elf"),
                ("4/uefi_dtb_b", "uefi_dtbs.elf"),
            ],
        ),
    ];
    for (board, pick) in [(SPLIT_LUN_BOARD, 0), (ONE_LUN_BOARD, 1)] {
        for (kind, payloads, split, one) in cases {
            let rig = Rig::new(SAHARA, board);
            stage_kind(&rig, "PAIR-0123456789ab", Some(kind), payloads);
            let o = rig.run(&["PAIR-0123456789ab"]);
            ok(&o);
            let calls = rig.calls();
            assert_eq!(calls.len(), 1, "{kind}: one qdl session");
            let want = if pick == 0 { split } else { one };
            assert_eq!(writes(&calls[0]), pairs(want), "{kind} on board {pick}");
        }
    }
}

#[test]
fn dry_run_reaches_qdl_as_its_own_dry_run() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 100)]);
    let o = rig.run(&["--dry-run", "FP-260921-222651"]);
    ok(&o);
    let calls = rig.calls();
    assert!(calls[0].args.iter().any(|a| a == "--dry-run"));
    assert!(!rig.stage.join("store").join("4_uefi_a").exists());
}

#[test]
fn a_kind_directory_must_list_exactly_its_payloads() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    let d = stage_kind(
        &rig,
        "PAIR-0123456789ab",
        Some("boot-chain"),
        &[("tz.mbn", 1), ("fip.elf", 1)],
    );
    // Order matters: the payload list is positional.
    sums(&d, "SHA256SUMS.flash", &["fip.elf", "tz.mbn"]);
    refused(
        &rig,
        &rig.run(&["PAIR-0123456789ab"]),
        "line 1 is not tz.mbn",
    );
    sums(&d, "SHA256SUMS.flash", &["tz.mbn"]);
    refused(
        &rig,
        &rig.run(&["PAIR-0123456789ab"]),
        "does not list exactly",
    );
    std::fs::remove_file(d.join("SHA256SUMS.flash")).unwrap();
    refused(
        &rig,
        &rig.run(&["PAIR-0123456789ab"]),
        "does not list exactly",
    );
}

#[test]
fn a_payload_that_changed_after_staging_is_refused() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    let d = stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 100)]);
    write(&d.join("fip.elf"), "tampered");
    refused(
        &rig,
        &rig.run(&["FP-260921-222651"]),
        "payload digest mismatch",
    );
}

#[test]
fn an_unknown_kind_is_refused() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(&rig, "FP-260921-222651", Some("xbl"), &[("xbl.elf", 1)]);
    refused(
        &rig,
        &rig.run(&["FP-260921-222651"]),
        "unsupported flash kind: xbl",
    );
}

#[test]
fn a_payload_over_its_size_limit_is_refused() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(
        &rig,
        "FP-260921-222651",
        Some("apdp"),
        &[("apdp.mbn", 65537)],
    );
    refused(
        &rig,
        &rig.run(&["FP-260921-222651"]),
        "apdp.mbn is 65537 bytes",
    );
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(&rig, "FP-260921-222651", Some("apdp"), &[("apdp.mbn", 0)]);
    refused(&rig, &rig.run(&["FP-260921-222651"]), "apdp.mbn is 0 bytes");
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(
        &rig,
        "FP-260921-222651",
        Some("apdp"),
        &[("apdp.mbn", 65536)],
    );
    ok(&rig.run(&["FP-260921-222651"]));
}

// ------------------------------------------------- shared refusals ------

#[test]
fn no_edl_serial_means_no_flash() {
    for serial in [
        None,
        Some(""),
        Some("\n"),
        Some("12 34"),
        Some("../x"),
        Some("$(reboot)"),
    ] {
        let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
        stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 1)]);
        let f = rig.stage.join("edl-serial");
        match serial {
            None => std::fs::remove_file(&f).unwrap(),
            Some(s) => write(&f, s),
        }
        refused(&rig, &rig.run(&["FP-260921-222651"]), "no device serial");
    }
}

#[test]
fn references_outside_the_boards_namespaces_are_refused() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 1)]);
    for r in [
        "FP-26092-222651",
        "fp-260921-222651",
        "FP-260921-222651/../x",
        "../FP-260921-222651",
        "PAIR-0123456789AB",
        "OTHER-1",
        "",
    ] {
        refused(&rig, &rig.run(&[r]), "");
    }
    refused(&rig, &rig.run(&["FP-260921-222652"]), "is not staged");
}

#[test]
fn a_runtime_that_drifted_from_its_manifest_is_refused() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 1)]);
    let qdl = rig.stage.join("runtime/qdl");
    write(&qdl, "another qdl");
    refused(&rig, &rig.run(&["FP-260921-222651"]), "runtime in");
    std::fs::remove_file(&qdl).unwrap();
    refused(&rig, &rig.run(&["FP-260921-222651"]), "runtime in");
}

#[test]
fn a_loader_that_drifted_from_its_manifest_is_refused() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 1)]);
    write(&rig.stage.join("firehose/xbl_sc.elf"), "another loader");
    refused(&rig, &rig.run(&["FP-260921-222651"]), "firehose in");
}

#[test]
fn a_manifest_the_image_does_not_ship_is_refused() {
    let rig = Rig::new(SAHARA, "");
    let board = format!(
        "stage {}\nruntime runtime qdl-v0-not-shipped\n{SPLIT_LUN_BOARD}",
        rig.stage.display()
    );
    write(&rig.board, board);
    stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 1)]);
    refused(&rig, &rig.run(&["FP-260921-222651"]), "is not shipped");
}

#[test]
fn a_board_file_with_an_error_is_refused() {
    for (bad, why) in [
        ("frobnicate yes", "unknown key frobnicate"),
        ("kind fip fip.elf uefi_a", "kind fip target uefi_a"),
        ("ref bogus X-1", "unknown mode bogus"),
        ("ref kind X-1 own-everything", "unknown ref flag"),
        ("reset maybe", "reset is yes or no"),
        ("window W 0/7 rw", "window address"),
        ("storage", "storage takes one value"),
    ] {
        let rig = Rig::new(SAHARA, &format!("{SPLIT_LUN_BOARD}{bad}\n"));
        stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 1)]);
        refused(&rig, &rig.run(&["FP-260921-222651"]), why);
    }
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    let text = std::fs::read_to_string(&rig.board).unwrap();
    write(
        &rig.board,
        text.lines().skip(1).collect::<Vec<_>>().join("\n"),
    );
    refused(&rig, &rig.run(&["FP-260921-222651"]), "stage is required");
    let missing = Command::new("bash")
        .arg(hook())
        .args(["--board", "/nonexistent", "FP-260921-222651"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
}

#[test]
fn a_stale_qdl_from_a_timed_out_hook_is_stopped_first() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 1)]);
    // Its command line carries "<runtime>/qdl ", as a real orphan's does.
    // A compound command, so sh stays the process instead of exec'ing sleep.
    let mut stale = Command::new("sh")
        .args(["-c", "sleep 30; :"])
        .arg(rig.stage.join("runtime/qdl"))
        .arg("--serial=x")
        .spawn()
        .unwrap();
    let o = rig.run(&["FP-260921-222651"]);
    ok(&o);
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("stopping stale qdl"),
        "{}",
        text(&o)
    );
    let status = stale.wait().unwrap();
    assert!(!status.success(), "the stale qdl was left running");
}

#[test]
fn one_flash_per_stage_and_qdl_never_inherits_the_lock() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 1)]);
    let lock = rig.stage.join(".qdl-flash.lock");
    write(&lock, "");
    // -o: only flock itself holds the lock, so killing it releases it.
    let mut holder = Command::new("flock")
        .arg("-o")
        .arg(&lock)
        .args(["sleep", "30"])
        .spawn()
        .unwrap();
    // flock takes the lock asynchronously; wait until it does.
    for _ in 0..100 {
        if !Command::new("flock")
            .args(["-n"])
            .arg(&lock)
            .arg("true")
            .status()
            .unwrap()
            .success()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    refused(
        &rig,
        &rig.run(&["FP-260921-222651"]),
        "another flash is running",
    );
    holder.kill().unwrap();
    holder.wait().unwrap();
    ok(&rig.run(&["FP-260921-222651"]));
    assert!(!rig.calls()[0].fd9_open, "qdl inherited the stage lock");
}

#[test]
fn a_failing_qdl_fails_the_hook() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_kind(&rig, "FP-260921-222651", None, &[("fip.elf", 1)]);
    write(&rig.stage.join("fake-exit"), "3");
    let o = rig.run(&["FP-260921-222651"]);
    assert_eq!(o.status.code(), Some(3), "{}", text(&o));
    assert!(!String::from_utf8_lossy(&o.stdout).contains("done"));
}

#[test]
fn check_verifies_the_stage_without_a_reference() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    ok(&rig.run(&["--check"]));
    write(&rig.stage.join("firehose/xbl_sc.elf"), "drifted");
    refused(&rig, &rig.run(&["--check"]), "firehose in");
}

// ---------------------------------------------------------------- xml ------

fn stage_xml(rig: &Rig, name: &str, own: bool, listed: bool) -> PathBuf {
    let d = rig.dir(name);
    let mut files = vec![
        "rawprogram0.xml",
        "rawprogram2.xml",
        "rawprogram10.xml",
        "patch0.xml",
        "patch10.xml",
        "patch2.xml",
        "a.bin",
    ];
    if own {
        write(&d.join("qsahara_device_programmer.xml"), "own programmer");
        write(&d.join("xbl_sc.elf"), "own loader");
        if listed {
            files.push("qsahara_device_programmer.xml");
        }
    }
    for f in &files {
        if !d.join(f).exists() {
            write(&d.join(f), format!("content {f}"));
        }
    }
    sums(&d, "SHA256SUMS", &files);
    d
}

#[test]
fn a_flat_build_runs_its_xml_in_order_with_its_own_programmer() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    let d = stage_xml(&rig, "STOCK-base-1", true, true);
    ok(&rig.run(&["STOCK-base-1"]));
    let c = &rig.calls()[0];
    assert_eq!(Path::new(&c.pwd), d.as_path());
    assert_eq!(
        Path::new(&programmer(c)),
        d.join("qsahara_device_programmer.xml")
    );
    assert!(c.args.contains(&format!("--include={}", d.display())));
    let names: Vec<String> = ops(c)
        .iter()
        .map(|p| Path::new(p).file_name().unwrap().to_string_lossy().into())
        .collect();
    assert_eq!(
        names,
        [
            "rawprogram0.xml",
            "rawprogram2.xml",
            "rawprogram10.xml",
            "patch0.xml",
            "patch2.xml",
            "patch10.xml"
        ]
    );
}

#[test]
fn a_flat_build_without_its_own_programmer_is_refused() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_xml(&rig, "STOCK-base-1", false, false);
    refused(
        &rig,
        &rig.run(&["STOCK-base-1"]),
        "must carry its own programmer",
    );
}

#[test]
fn a_programmer_outside_sha256sums_is_refused() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_xml(&rig, "IMG-x", true, false);
    refused(&rig, &rig.run(&["IMG-x"]), "is not in SHA256SUMS");
}

#[test]
fn an_image_set_without_a_programmer_uses_the_verified_firehose_set() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    stage_xml(&rig, "IMG-x", false, false);
    ok(&rig.run(&["IMG-x"]));
    let c = &rig.calls()[0];
    assert_eq!(Path::new(&c.pwd), rig.stage.join("firehose"));
}

#[test]
fn an_xml_set_that_does_not_verify_or_has_no_rawprogram_is_refused() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    let d = stage_xml(&rig, "IMG-x", false, false);
    write(&d.join("a.bin"), "changed");
    refused(&rig, &rig.run(&["IMG-x"]), "SHA256SUMS does not verify");
    let d = rig.dir("IMG-y");
    write(&d.join("a.bin"), "x");
    sums(&d, "SHA256SUMS", &["a.bin"]);
    refused(&rig, &rig.run(&["IMG-y"]), "has no rawprogram XML");
}

// ------------------------------------------------- parts and reset ------

const DEVPRG: &[&str] = &["xbl_s_devprg_ns.melf"];

#[test]
fn a_partition_list_is_one_session_of_label_writes() {
    let rig = Rig::new(DEVPRG, RESET_BOARD);
    let d = rig.dir("FP-261108-2127");
    write(&d.join("a.img"), "a");
    write(&d.join("b.img"), "b");
    write(
        &d.join("partitions.txt"),
        "# comment\n0/userdata a.img\n\n4/hyp_a b.img\n",
    );
    sums(&d, "SHA256SUMS", &["a.img", "b.img"]);
    ok(&rig.run(&["FP-261108-2127"]));
    let calls = rig.calls();
    assert_eq!(calls.len(), 1);
    let c = &calls[0];
    assert_eq!(
        writes(c),
        pairs(&[("0/userdata", "a.img"), ("4/hyp_a", "b.img")])
    );
    assert!(
        !c.args.iter().any(|a| a == "--skip-reset"),
        "this board resets"
    );
    assert!(!c.args.iter().any(|a| a == "--debug"));
    assert_eq!(
        Path::new(&programmer(c)),
        rig.stage.join("firehose/xbl_s_devprg_ns.melf")
    );
}

#[test]
fn a_partition_list_refuses_unlisted_files_and_raw_sectors() {
    let rig = Rig::new(DEVPRG, RESET_BOARD);
    let d = rig.dir("FP-261108-1");
    write(&d.join("a.img"), "a");
    write(&d.join("b.img"), "b");
    sums(&d, "SHA256SUMS", &["a.img"]);
    write(
        &d.join("partitions.txt"),
        "0/userdata a.img\n4/hyp_a b.img\n",
    );
    refused(
        &rig,
        &rig.run(&["FP-261108-1"]),
        "b.img is not in SHA256SUMS",
    );
    write(&d.join("partitions.txt"), "0/0+6 a.img\n");
    refused(&rig, &rig.run(&["FP-261108-1"]), "bad line");
    write(&d.join("partitions.txt"), "0/userdata ../a.img\n");
    refused(&rig, &rig.run(&["FP-261108-1"]), "bad line");
    write(&d.join("partitions.txt"), "0/userdata missing.img\n");
    refused(
        &rig,
        &rig.run(&["FP-261108-1"]),
        "missing.img is not staged",
    );
    std::fs::remove_file(d.join("partitions.txt")).unwrap();
    refused(&rig, &rig.run(&["FP-261108-1"]), "needs partitions.txt");
}

#[test]
fn xml_or_parts_takes_xml_when_the_set_has_it() {
    let rig = Rig::new(DEVPRG, RESET_BOARD);
    let d = rig.dir("FP-261108-9");
    write(&d.join("rawprogram0.xml"), "r");
    write(&d.join("partitions.txt"), "0/userdata a.img\n");
    sums(&d, "SHA256SUMS", &["rawprogram0.xml"]);
    ok(&rig.run(&["FP-261108-9"]));
    let o = ops(&rig.calls()[0]);
    assert_eq!(o, [d.join("rawprogram0.xml").display().to_string()]);
}

#[test]
fn firehose_programmer_ignores_a_programmer_in_the_set() {
    let rig = Rig::new(DEVPRG, RESET_BOARD);
    let d = rig.dir("STOCK-LA-1");
    write(&d.join("rawprogram0.xml"), "r");
    write(&d.join("xbl_s_devprg_ns.melf"), "the set's own");
    sums(
        &d,
        "SHA256SUMS",
        &["rawprogram0.xml", "xbl_s_devprg_ns.melf"],
    );
    ok(&rig.run(&["STOCK-LA-1"]));
    assert_eq!(
        Path::new(&programmer(&rig.calls()[0])),
        rig.stage.join("firehose/xbl_s_devprg_ns.melf")
    );
}

// --------------------------------------------------------------- read ------

#[test]
fn a_read_back_writes_its_files_and_their_digests() {
    let rig = Rig::new(DEVPRG, RESET_BOARD);
    let d = rig.dir("READ-hyp");
    write(&d.join("xbl_s_devprg_ns.melf"), "own");
    write(
        &d.join("partitions.txt"),
        "4/hyp_a hyp_a.bin\n0/0+6 lun0-gpt.bin\n",
    );
    sums(&d, "SHA256SUMS", &["xbl_s_devprg_ns.melf"]);
    ok(&rig.run(&["READ-hyp"]));
    let c = &rig.calls()[0];
    assert_eq!(Path::new(&programmer(c)), d.join("xbl_s_devprg_ns.melf"));
    assert_eq!(
        ops(c),
        [
            "read".to_string(),
            "4/hyp_a".into(),
            d.join("hyp_a.bin").display().to_string(),
            "read".into(),
            "0/0+6".into(),
            d.join("lun0-gpt.bin").display().to_string(),
        ]
    );
    assert_eq!(
        std::fs::metadata(d.join("lun0-gpt.bin")).unwrap().len(),
        6 * 4096
    );
    let read = std::fs::read_to_string(d.join("SHA256SUMS.read")).unwrap();
    assert!(
        read.contains("hyp_a.bin") && read.contains("lun0-gpt.bin"),
        "{read}"
    );
    // Never over an earlier result.
    refused(&rig, &rig.run(&["READ-hyp"]), "hyp_a.bin already exists");
}

#[test]
fn a_read_back_that_needs_its_own_programmer_refuses_without_one() {
    let rig = Rig::new(DEVPRG, RESET_BOARD);
    let d = rig.dir("READ-x");
    write(&d.join("partitions.txt"), "4/hyp_a hyp_a.bin\n");
    sums(&d, "SHA256SUMS", &["partitions.txt"]);
    refused(&rig, &rig.run(&["READ-x"]), "must carry its own programmer");
}

#[test]
fn a_read_back_falls_back_to_the_firehose_set_where_the_board_allows() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    let d = rig.dir("READ-gpt-probe");
    write(
        &d.join("partitions.txt"),
        "0/0+6 lun0-gpt.bin\n5/0+6 lun5-gpt.bin\n",
    );
    sums(&d, "SHA256SUMS", &["partitions.txt"]);
    ok(&rig.run(&["--dry-run", "READ-gpt-probe"]));
    assert_eq!(Path::new(&rig.calls()[0].pwd), rig.stage.join("firehose"));
    assert!(
        !d.join("SHA256SUMS.read").exists(),
        "a dry run reads nothing, so it has no digests to write"
    );
    write(&d.join("partitions.txt"), "0/0+6 ../escape.bin\n");
    sums(&d, "SHA256SUMS", &["partitions.txt"]);
    refused(&rig, &rig.run(&["READ-gpt-probe"]), "bad line");
}

// -------------------------------------------------------------- windows ----

const NOR_PROG: &[&str] = &["prog_firehose_ddr.elf"];

fn stage_window(rig: &Rig, name: &str, part: &str, payload: usize) -> PathBuf {
    let d = rig.dir(name);
    write(&d.join("sail_hyp.bin"), vec![7u8; payload]);
    write(&d.join("part.txt"), format!("{part}\n"));
    sums(&d, "SHA256SUMS", &["sail_hyp.bin"]);
    d
}

#[test]
fn a_window_write_pads_to_sectors_and_proves_the_readback() {
    let rig = Rig::new(NOR_PROG, NOR_BOARD);
    let d = stage_window(&rig, "SAILZ-174", "BACKUP", 5000);
    let o = rig.run(&["SAILZ-174"]);
    ok(&o);
    let c = &rig.calls()[0];
    assert!(c.args.iter().any(|a| a == "--storage=spinor"));
    assert_eq!(
        ops(c),
        [
            "write".to_string(),
            "0/1781+2".into(),
            d.join("write.img").display().to_string(),
            "read".into(),
            "0/1781+2".into(),
            d.join("readback.img").display().to_string(),
        ]
    );
    assert_eq!(std::fs::metadata(d.join("write.img")).unwrap().len(), 8192);
    assert!(String::from_utf8_lossy(&o.stdout).contains("READBACK MATCH BACKUP sectors=2"));
}

#[test]
fn a_window_readback_mismatch_fails() {
    let rig = Rig::new(NOR_PROG, NOR_BOARD);
    stage_window(&rig, "SAILZ-1", "PRIMARY", 10);
    write(&rig.stage.join("corrupt-readback"), "");
    let o = rig.run(&["SAILZ-1"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(String::from_utf8_lossy(&o.stdout).contains("READBACK MISMATCH"));
}

#[test]
fn a_window_write_refuses_what_does_not_fit_or_is_not_writable() {
    let rig = Rig::new(NOR_PROG, NOR_BOARD);
    stage_window(&rig, "SAILZ-2", "PRIMARY", 4 * 4096 + 1);
    refused(&rig, &rig.run(&["SAILZ-2"]), "5 sectors, max 4");
    stage_window(&rig, "SAILZ-3", "FULL", 10);
    refused(&rig, &rig.run(&["SAILZ-3"]), "refusing to write FULL");
    stage_window(&rig, "SAILZ-4", "OTHER", 10);
    refused(&rig, &rig.run(&["SAILZ-4"]), "no window OTHER");
    let d = stage_window(&rig, "SAILZ-5", "PRIMARY", 10);
    write(&d.join("part.txt"), "PRIMARY\nBACKUP\n");
    refused(&rig, &rig.run(&["SAILZ-5"]), "exactly one window");
    let d = stage_window(&rig, "SAILZ-6", "PRIMARY", 10);
    write(&d.join("other.bin"), "o");
    sums(&d, "SHA256SUMS", &["sail_hyp.bin", "other.bin"]);
    refused(
        &rig,
        &rig.run(&["SAILZ-6"]),
        "exactly one line for sail_hyp.bin",
    );
    let d = stage_window(&rig, "SAILZ-7", "PRIMARY", 10);
    write(&d.join("sail_hyp.bin"), "changed");
    refused(&rig, &rig.run(&["SAILZ-7"]), "payload digest mismatch");
    let d = stage_window(&rig, "SAILZ-8", "PRIMARY", 10);
    write(&d.join("sail_hyp.bin"), "");
    sums(&d, "SHA256SUMS", &["sail_hyp.bin"]);
    refused(&rig, &rig.run(&["SAILZ-8"]), "is 0 bytes");
}

#[test]
fn a_window_read_takes_the_whole_window_and_checks_its_length() {
    let rig = Rig::new(NOR_PROG, NOR_BOARD);
    let d = rig.dir("NORREAD-full");
    write(&d.join("part.txt"), "FULL\n");
    ok(&rig.run(&["NORREAD-full"]));
    assert_eq!(
        ops(&rig.calls()[0]),
        [
            "read".to_string(),
            "0/0+16".into(),
            d.join("FULL.bin").display().to_string()
        ]
    );
    assert_eq!(
        std::fs::metadata(d.join("FULL.bin")).unwrap().len(),
        16 * 4096
    );
    assert!(d.join("SHA256SUMS.read").exists());
    refused(&rig, &rig.run(&["NORREAD-full"]), "FULL.bin already exists");

    let d = rig.dir("NORREAD-short");
    write(&d.join("part.txt"), "PRIMARY\n");
    write(&rig.stage.join("short-read"), "");
    let o = rig.run(&["NORREAD-short"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(said(&o).contains("short read of PRIMARY"));
}

// ------------------------------------------------------------ fastboot ----

fn with_fastboot(rig: &Rig) {
    let fb = rig.stage.join("fbruntime");
    exec(&fb.join("ld-linux-x86-64.so.2"), FAKE_LOADER);
    write(&fb.join("fastboot"), "fastboot stand-in");
    manifest(
        &fb,
        &["fastboot", "ld-linux-x86-64.so.2"],
        &rig.manifest_dir().join("fastboot.sha256"),
    );
    write(&rig.stage.join("fastboot-serial"), "fb0001\n");
    rig.set_board(&format!(
        "{RESET_BOARD}fastboot_runtime fbruntime {}\n",
        rig.manifest_dir().join("fastboot.sha256").display()
    ));
}

#[test]
fn a_boot_image_is_booted_once_over_fastboot() {
    let rig = Rig::new(DEVPRG, RESET_BOARD);
    with_fastboot(&rig);
    let d = rig.dir("BOOT-initrd-1");
    write(&d.join("boot.img"), "kernel");
    sums(&d, "SHA256SUMS", &["boot.img"]);
    ok(&rig.run(&["BOOT-initrd-1"]));
    let calls = rig.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].prog, "fastboot");
    assert_eq!(calls[0].args, ["-s", "fb0001", "boot", "boot.img"]);
    assert!(!calls[0].fd9_open);

    let o = rig.run(&["--dry-run", "BOOT-initrd-1"]);
    ok(&o);
    assert!(rig.calls().is_empty(), "a dry run must not reach fastboot");
    assert!(String::from_utf8_lossy(&o.stdout).contains("fastboot -s fb0001 boot boot.img"));
}

#[test]
fn a_boot_image_must_verify_and_so_must_fastboot() {
    let rig = Rig::new(DEVPRG, RESET_BOARD);
    with_fastboot(&rig);
    let d = rig.dir("BOOT-x");
    write(&d.join("boot.img"), "kernel");
    write(&d.join("other"), "o");
    sums(&d, "SHA256SUMS", &["other"]);
    refused(&rig, &rig.run(&["BOOT-x"]), "needs boot.img listed");
    sums(&d, "SHA256SUMS", &["boot.img"]);
    write(&rig.stage.join("fbruntime/fastboot"), "another fastboot");
    refused(&rig, &rig.run(&["BOOT-x"]), "fastboot_runtime in");
    let rig = Rig::new(DEVPRG, RESET_BOARD);
    let d = rig.dir("BOOT-y");
    write(&d.join("boot.img"), "kernel");
    sums(&d, "SHA256SUMS", &["boot.img"]);
    refused(
        &rig,
        &rig.run(&["BOOT-y"]),
        "fastboot-boot needs fastboot_runtime",
    );
}

// --------------------------------------------------------------- digest ----

fn digest_rig() -> Rig {
    let rig = Rig::new(NOR_PROG, &digest_board());
    rig.dir("work");
    rig
}

fn stage_digest(rig: &Rig, name: &str, bytes: usize) -> String {
    let p = rig.stage.join("work").join(name);
    write(&p, vec![9u8; bytes]);
    sha256(&p)
}

#[test]
fn a_digest_image_is_written_from_a_snapshot_and_verified_on_the_device() {
    let rig = digest_rig();
    let sum = stage_digest(&rig, "efi.img", 4096);
    let r = format!("v1:efi:{sum}:efi.img");
    let o = rig.run(&["--device", CONSOLE, &r]);
    ok(&o);
    let calls = rig.calls();
    assert_eq!(calls.len(), 2, "a write session and a readback session");
    let w = ops(&calls[0]);
    assert_eq!(w[0..2], ["write".to_string(), "0/efi".into()]);
    assert!(
        w[2].contains("/runs/run-") && w[2].ends_with("/efi.bin"),
        "{w:?}"
    );
    assert!(
        !w[2].contains("/work/"),
        "qdl must read the snapshot: {w:?}"
    );
    assert_eq!(ops(&calls[1]), ["sha256".to_string(), "0/efi".into()]);
    for c in &calls {
        assert!(c.args.iter().any(|a| a == "--storage=emmc"));
        assert!(c.args.iter().any(|a| a == "--skip-reset"));
    }
    assert!(String::from_utf8_lossy(&o.stdout).contains(&format!("VERIFIED_SHA256={sum}")));
}

#[test]
fn a_digest_rootfs_reads_back_its_pinned_window() {
    let rig = digest_rig();
    let sum = stage_digest(&rig, "rootfs.img", 2048);
    ok(&rig.run(&["--device", CONSOLE, &format!("v1:rootfs:{sum}:rootfs.img")]));
    let calls = rig.calls();
    assert_eq!(ops(&calls[0])[1], "0/rootfs");
    assert_eq!(ops(&calls[1]), ["sha256".to_string(), "0/2306184+4".into()]);
}

#[test]
fn a_digest_image_is_refused_on_any_doubt() {
    let rig = digest_rig();
    let sum = stage_digest(&rig, "efi.img", 4096);
    let good = format!("v1:efi:{sum}:efi.img");
    refused(&rig, &rig.run(&[&good]), "wrong console");
    refused(
        &rig,
        &rig.run(&["--device", "/dev/other", &good]),
        "wrong console",
    );
    let wrong = format!("v1:efi:{}:efi.img", "0".repeat(64));
    refused(
        &rig,
        &rig.run(&["--device", CONSOLE, &wrong]),
        "image digest mismatch",
    );
    let small = stage_digest(&rig, "small.img", 512);
    refused(
        &rig,
        &rig.run(&["--device", CONSOLE, &format!("v1:efi:{small}:small.img")]),
        "must be exactly 4096 bytes",
    );
    let big = stage_digest(&rig, "big.img", 8704);
    refused(
        &rig,
        &rig.run(&["--device", CONSOLE, &format!("v1:rootfs:{big}:big.img")]),
        "must be 1..8192 bytes",
    );
    let odd = stage_digest(&rig, "odd.img", 1000);
    refused(
        &rig,
        &rig.run(&["--device", CONSOLE, &format!("v1:rootfs:{odd}:odd.img")]),
        "not whole 512-byte sectors",
    );
    refused(
        &rig,
        &rig.run(&["--device", CONSOLE, &format!("v1:boot:{sum}:efi.img")]),
        "no digest target boot",
    );
    refused(
        &rig,
        &rig.run(&["--device", CONSOLE, &format!("v1:efi:{sum}:../efi.img")]),
        "",
    );
    std::os::unix::fs::symlink(
        rig.stage.join("work/efi.img"),
        rig.stage.join("work/link.img"),
    )
    .unwrap();
    refused(
        &rig,
        &rig.run(&["--device", CONSOLE, &format!("v1:efi:{sum}:link.img")]),
        "symlink",
    );
}

#[test]
fn a_digest_readback_that_disagrees_or_is_ambiguous_fails() {
    for fake in [
        "0".repeat(64),
        format!("{}\n{}", "a".repeat(64), "b".repeat(64)),
        String::new(),
    ] {
        let rig = digest_rig();
        let sum = stage_digest(&rig, "efi.img", 4096);
        write(&rig.stage.join("fake-sha"), format!("{fake}\n"));
        let o = rig.run(&["--device", CONSOLE, &format!("v1:efi:{sum}:efi.img")]);
        assert_eq!(o.status.code(), Some(1), "{}", text(&o));
        assert!(said(&o).contains("readback digest mismatch"));
    }
}

// ------------------------------------------------------------ shipping ----

/// Every checked-in manifest is something `sha256sum -c --strict` accepts, and
/// a firehose manifest is named by its own content so a board file pins it.
#[test]
fn the_shipped_manifests_are_well_formed_and_content_named() {
    let line = regex::Regex::new(r"^[0-9a-f]{64}  [A-Za-z0-9._+-]+$").unwrap();
    let mut seen = 0;
    for e in std::fs::read_dir(manifests_dir()).unwrap() {
        let p = e.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.ends_with(".sha256"), "{name}");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.is_empty() && text.ends_with('\n'), "{name}");
        let files: Vec<&str> = text
            .lines()
            .map(|l| {
                assert!(line.is_match(l), "{name}: {l:?}");
                l.split_at(66).1
            })
            .collect();
        let mut sorted = files.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(files, sorted, "{name}: sorted, no duplicates");
        if let Some(h) = name.strip_prefix("firehose-") {
            assert_eq!(&sha256(&p)[..12], h.trim_end_matches(".sha256"), "{name}");
        }
        seen += 1;
    }
    assert!(seen > 0);
}

#[test]
fn the_hook_is_executable() {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(hook()).unwrap().permissions().mode();
    assert!(mode & 0o111 != 0, "hooks/qdl-flash must be executable");
}

// ----------------------------------------------------- board vs its GPT ----

/// `lun<N>-gpt.bin` as a read of `<N>/0+6` returns it: protective MBR, header
/// at sector 1, 128-byte entries from sector 2.
fn gpt_dump(path: &Path, labels: &[&str]) {
    const SS: usize = 4096;
    let mut d = vec![0u8; 6 * SS];
    d[SS..SS + 8].copy_from_slice(b"EFI PART");
    d[SS + 72..SS + 80].copy_from_slice(&2u64.to_le_bytes());
    d[SS + 80..SS + 84].copy_from_slice(&128u32.to_le_bytes());
    d[SS + 84..SS + 88].copy_from_slice(&128u32.to_le_bytes());
    for (i, name) in labels.iter().enumerate() {
        let e = 2 * SS + i * 128;
        d[e] = 0xa2; // any non-zero type GUID marks the entry used
        for (j, u) in name.encode_utf16().enumerate() {
            d[e + 56 + 2 * j..e + 58 + 2 * j].copy_from_slice(&u.to_le_bytes());
        }
    }
    write(path, d);
}

/// The GPT of a board that splits its A/B slots over LUNs 4 and 5, with apdp
/// on LUNs 1 and 2 and efi on LUN 0.
fn split_lun_gpt(dir: &Path) {
    gpt_dump(&dir.join("lun0-gpt.bin"), &["efi", "userdata"]);
    gpt_dump(&dir.join("lun1-gpt.bin"), &["xbl_a", "apdp_a"]);
    gpt_dump(&dir.join("lun2-gpt.bin"), &["xbl_b", "apdp_b"]);
    gpt_dump(&dir.join("lun4-gpt.bin"), &["tz_a", "uefi_a", "uefi_dtb_a"]);
    gpt_dump(&dir.join("lun5-gpt.bin"), &["tz_b", "uefi_b", "uefi_dtb_b"]);
}

#[test]
fn a_board_map_that_matches_its_gpt_checks_clean() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    let gpt = rig.dir("READ-gpt");
    split_lun_gpt(&gpt);
    let o = rig.run(&["--check", "--gpt", gpt.to_str().unwrap()]);
    ok(&o);
}

/// The regression: a board file copied from a board of the same SoC whose
/// flat build keeps both slots on LUN 4 names B-slot and apdp targets this
/// board does not have. qdl --dry-run never resolves labels, so only the
/// board's own GPT catches it.
#[test]
fn a_board_map_copied_from_another_layout_fails_against_the_boards_gpt() {
    let rig = Rig::new(SAHARA, ONE_LUN_BOARD);
    let gpt = rig.dir("READ-gpt");
    split_lun_gpt(&gpt);
    let o = rig.run(&["--check", "--gpt", gpt.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    let s = said(&o);
    for t in ["4/uefi_b", "4/tz_b", "4/apdp_a", "4/apdp_b", "4/uefi_dtb_b"] {
        assert!(
            s.contains(&format!("{t} is not on this board's GPT")),
            "{t}: {s}"
        );
    }
    for t in ["4/uefi_a ", "4/tz_a ", "0/efi "] {
        assert!(!s.contains(t), "{t} is on the GPT: {s}");
    }
}

#[test]
fn a_kind_whose_partition_the_board_lacks_fails_the_gpt_check() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    let gpt = rig.dir("READ-gpt");
    split_lun_gpt(&gpt);
    gpt_dump(&gpt.join("lun0-gpt.bin"), &["lv_boot_a", "userdata"]);
    let o = rig.run(&["--check", "--gpt", gpt.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(said(&o).contains("kind efi: 0/efi is not on this board's GPT"));
}

#[test]
fn a_gpt_check_needs_a_read_of_every_lun_it_names() {
    let rig = Rig::new(SAHARA, SPLIT_LUN_BOARD);
    let gpt = rig.dir("READ-gpt");
    split_lun_gpt(&gpt);
    std::fs::remove_file(gpt.join("lun5-gpt.bin")).unwrap();
    refused(
        &rig,
        &rig.run(&["--check", "--gpt", gpt.to_str().unwrap()]),
        "no GPT read of LUN 5",
    );
    write(&gpt.join("lun5-gpt.bin"), vec![0u8; 6 * 4096]);
    refused(
        &rig,
        &rig.run(&["--check", "--gpt", gpt.to_str().unwrap()]),
        "holds no GPT",
    );
    refused(
        &rig,
        &rig.run(&["--gpt", gpt.to_str().unwrap(), "FP-260921-222651"]),
        "--gpt goes with --check",
    );
}
