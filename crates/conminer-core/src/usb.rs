//! USB truth about a board: is it in EDL, and is what we see actually alive?
//!
//! Two board behaviours forced this, both measured on the rig, and both were
//! costing the operator a manual diagnosis every time:
//!
//!   * **A board in EDL looks exactly like a board that is off.** The console is
//!     silent by design, so conminer's view is "connected, received nothing" --
//!     and on a controller with no power-sense pins (the Bughopper) that is
//!     indistinguishable from powered-off. The only honest signal is out of band:
//!     Qualcomm's QDL/Sahara gadget enumerating as `05c6:9008`.
//!   * **Presence is not liveness.** After a verifiably-off board, its gadget
//!     stayed listed for minutes with cached descriptors while any real read
//!     failed ("Resource temporarily unavailable") and the hub never saw a
//!     detach. Root cause is that board's faulty Type-C controller, but the
//!     consequence is conminer's problem: a stale entry makes the next EDL or
//!     flash session target a device that is not there.
//!
//! So enumeration alone is never trusted here. A device counts as present only
//! when it still answers.

/// Qualcomm's vendor id, shared by the PBL's EDL/Sahara gadget.
const QCOM_VID: u16 = 0x05c6;
/// Product ids the PBL exposes in emergency download mode.
const QDL_PIDS: &[u16] = &[0x9008, 0x900e, 0x9025];

/// One USB device as conminer cares about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub vendor_id: u16,
    pub product_id: u16,
    pub bus: u8,
    pub address: u8,
    /// Kernel port path -- `2-3.1.2`, the hub topology this device is plugged
    /// into. §L6: a bus is shared, a port path is not, so this is what makes a
    /// USB observation attributable to ONE board.
    pub port_path: Option<String>,
    /// Whether the device answered, could not answer, or could not be asked.
    pub liveness: Liveness,
}

/// What a liveness probe actually established.
///
/// THREE STATES, because two cannot express the case that matters: "I was not
/// allowed to ask" is not "the device is dead", and collapsing them is how a
/// perfectly healthy board gets reported as a ghost. This is not hypothetical --
/// the same conflation in `lsusb -v` (which prints "cannot read device status"
/// when it lacks permission, in exactly the words it uses for a real zombie)
/// cost an evening of chasing a device that was fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Answered a descriptor read. Definitely there.
    Alive,
    /// Could be asked and did not answer. Definitely a zombie.
    Dead,
    /// Could not be asked -- no permission, no node, no usbfs. Says nothing
    /// about the device, and must never be reported as either of the above.
    Unknown,
}

/// Is this vendor/product pair the Qualcomm emergency-download gadget?
///
/// Split out from [`UsbDevice::is_qdl`] because discovery has to answer the same
/// question about a bare tty, long before anything builds a `UsbDevice`: a board
/// in EDL exposes this gadget, and on some boards it carries no USB serial
/// number, so it arrives as a positional console rather than a named one.
pub fn is_qdl_id(vendor_id: u16, product_id: u16) -> bool {
    vendor_id == QCOM_VID && QDL_PIDS.contains(&product_id)
}

impl UsbDevice {
    /// Is this the Qualcomm emergency-download gadget?
    pub fn is_qdl(&self) -> bool {
        is_qdl_id(self.vendor_id, self.product_id)
    }

    /// Answered a descriptor read.
    pub fn responsive(&self) -> bool {
        self.liveness == Liveness::Alive
    }

    /// Listed but not answering: cached descriptors for a device that is gone.
    ///
    /// `Unknown` is deliberately NOT a zombie. Reporting a device as dead
    /// because we could not open it makes every device in an under-privileged
    /// container a ghost, and the resulting "clear the zombies" advice is then
    /// aimed at hardware that was never broken.
    pub fn is_zombie(&self) -> bool {
        self.liveness == Liveness::Dead
    }

    /// We could not determine this device's state at all.
    pub fn undetermined(&self) -> bool {
        self.liveness == Liveness::Unknown
    }
}

/// A board is in EDL only if a QDL gadget is present AND still answering.
///
/// The liveness half is not pedantry: a stale 9008 left behind by a previous
/// session would otherwise report "in EDL" for a board that is powered off, and
/// the caller's next move (flash it) would target nothing.
pub fn in_edl(devices: &[UsbDevice]) -> bool {
    devices.iter().any(|d| d.is_qdl() && d.responsive())
}

/// Is THIS BOARD in EDL -- a live QDL gadget on its own hub ports?
///
/// §L6 scoped zombie attribution to a board's ports and left EDL bus-wide, so on
/// a bench with two boards, one of them sitting in download mode answered for
/// both. "A board is in EDL" and "this board is in EDL" are different claims,
/// and only the second one is worth acting on: it decides whether a flash goes
/// ahead, and flashing the wrong board is not a recoverable mistake.
///
/// With no ports known for the board, the honest answer is the bus-wide one --
/// returned here, and reported by the caller under a name that says attribution
/// is unknown, rather than quietly implying it was checked.
pub fn in_edl_on_ports(devices: &[UsbDevice], ports: &[String]) -> bool {
    if ports.is_empty() {
        return in_edl(devices);
    }
    devices
        .iter()
        .any(|d| d.is_qdl() && d.responsive() && on_ports(d, ports))
}

/// A recovery-gadget signature: a USB vendor id and an OPTIONAL product id.
///
/// EDL/QDL is only ONE flashing shape. fastboot, DFU, SAM-BA, DevProg and the
/// next tool nobody has met yet each enumerate as a different USB identity, so
/// the set of "the board is in a flash/recovery mode" gadgets is DATA, not code:
/// a signature list the capture layer consults, extensible from config without
/// touching this file. `pid == None` matches a whole vendor family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GadgetSig {
    pub vid: u16,
    pub pid: Option<u16>,
}

impl GadgetSig {
    pub fn matches(&self, vid: u16, pid: u16) -> bool {
        self.vid == vid && self.pid.map_or(true, |p| p == pid)
    }

    /// Parse `"05c6:9008"` or `"05c6:*"` (whole-vendor). Case-insensitive hex,
    /// with or without a `0x` prefix. Returns None on anything malformed so a
    /// typo in config is dropped loudly by the caller, never matched by accident.
    pub fn parse(s: &str) -> Option<Self> {
        let (v, p) = s.split_once(':')?;
        let hex = |x: &str| u16::from_str_radix(x.trim().trim_start_matches("0x"), 16).ok();
        let vid = hex(v)?;
        let pid = match p.trim() {
            "*" | "" => None,
            other => Some(hex(other)?),
        };
        Some(GadgetSig { vid, pid })
    }
}

/// The recovery/flash-mode gadgets known by default: Qualcomm EDL/Sahara/QDL
/// (which also carries Firehose/DevProg), Google/Android fastboot, and STM DFU.
/// A bench adds its own by listing them in config; nothing here needs editing.
pub fn default_recovery_signatures() -> Vec<GadgetSig> {
    [
        (QCOM_VID, Some(0x9008)), // Sahara / QDL / Firehose
        (QCOM_VID, Some(0x900e)),
        (QCOM_VID, Some(0x9025)),
        (0x18d1, Some(0xd00d)), // Android fastboot (Google)
        (0x0483, Some(0xdf11)), // STM DFU
    ]
    .into_iter()
    .map(|(vid, pid)| GadgetSig { vid, pid })
    .collect()
}

/// Cheap PRESENCE enumeration: `(vid, pid, port_path)` for every device on the
/// bus, WITHOUT the per-device liveness probe `scan()` does.
///
/// `scan()` opens each device and reads a descriptor (~300 ms each, ~6.6 s on a
/// populated host -- the cost behind report #20), because it must tell a live
/// gadget from a stale one. Presence needs neither: the capture loop asks "is a
/// recovery gadget on this board's port" on every commit tick, and a sysfs walk
/// is the right tool for a hot path. Respects the test fixture like `scan()`.
pub fn list_present() -> Vec<(u16, u16, Option<String>)> {
    if let Some(fixture) = fixture_scan() {
        return fixture
            .iter()
            .map(|d| (d.vendor_id, d.product_id, d.port_path.clone()))
            .collect();
    }
    present_from_sysfs()
}

#[cfg(target_os = "linux")]
fn present_from_sysfs() -> Vec<(u16, u16, Option<String>)> {
    let Ok(list) = nusb::list_devices() else {
        return Vec::new();
    };
    list.map(|d| {
        (
            d.vendor_id(),
            d.product_id(),
            d.sysfs_path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
        )
    })
    .collect()
}

#[cfg(not(target_os = "linux"))]
fn present_from_sysfs() -> Vec<(u16, u16, Option<String>)> {
    Vec::new()
}

/// Is a recovery/flash-mode gadget present on any of this board's own ports?
///
/// The generic form of `in_edl_on_ports`: matches a SIGNATURE LIST rather than
/// hard-coded QDL, so a board entering fastboot or DFU is caught by the same
/// path. Presence-only (no liveness), because it runs on every tick and a gadget
/// listed on the board's port is enough to say "not the normal console". A stale
/// entry keeps the state a little longer, which is the safe direction: the
/// capture loop revalidates and clears it once the real console returns.
pub fn recovery_gadget_on_ports(ports: &[String], sigs: &[GadgetSig]) -> bool {
    recovery_present_in(&list_present(), ports, sigs)
}

/// The pure decision behind `recovery_gadget_on_ports`, over an already-gathered
/// device list `(vid, pid, port_path)`. Split out so it is testable without a
/// live bus or the process-wide fixture env.
pub fn recovery_present_in(
    present: &[(u16, u16, Option<String>)],
    ports: &[String],
    sigs: &[GadgetSig],
) -> bool {
    if ports.is_empty() || sigs.is_empty() {
        return false;
    }
    present.iter().any(|(vid, pid, port)| {
        port.as_deref().is_some_and(|path| port_on_any(path, ports))
            && sigs.iter().any(|sg| sg.matches(*vid, *pid))
    })
}

/// Same containment rule `on_ports` uses, on a raw port path string.
fn port_on_any(path: &str, ports: &[String]) -> bool {
    ports.iter().any(|p| {
        path == p
            || path
                .strip_prefix(p.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// QDL gadgets we could not probe at all.
///
/// Separate from `stale_qdl` on purpose: a gadget we were not allowed to open
/// is not evidence of a dirty bus, and counting it as one would send a caller
/// power-cycling a hub over a permissions problem.
pub fn undetermined_qdl(devices: &[UsbDevice]) -> usize {
    devices
        .iter()
        .filter(|d| d.is_qdl() && d.undetermined())
        .count()
}

/// The result of watching USB for a QDL gadget over a window of time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdlProbe {
    /// A live QDL gadget was seen.
    pub in_edl: bool,
    /// How long we watched before answering.
    pub waited_ms: i64,
    /// True when the full window elapsed with no gadget, so "not in EDL" is a
    /// claim about the board rather than about one instant.
    pub settled: bool,
    /// QDL gadgets that are LISTED but do not answer, at the final look.
    ///
    /// Measured on the ADP: `off` while the board was in EDL left `05c6:9008`
    /// enumerated as the same device number, no longer answering
    /// ("cannot read device status, Resource temporarily unavailable") because
    /// that board's Type-C controller never signals detach. Not a live EDL --
    /// but very much not a clean bus either, and the difference is the caller's
    /// next move: a stale 9008 is what the next flash session will target.
    pub stale_qdl: usize,
}

impl EdlProbe {
    /// Can the caller state as fact that the board is not in EDL?
    ///
    /// Only after a settled window. A single negative scan means "no gadget
    /// right now", which is also exactly what a re-enumerating board looks like.
    pub fn excludes_edl(&self) -> bool {
        !self.in_edl && self.settled
    }
}

/// Watch USB for a QDL gadget, returning as soon as one answers.
///
/// EDL ENTRY IS NOT INSTANT, and one scan cannot tell "not in EDL" apart from
/// "in EDL, mid-re-enumeration". Measured on the ADP: an `off` issued while the
/// board was in EDL warm-reset the PBL, the QDL gadget dropped off the bus, and
/// a scan two seconds later saw nothing -- so the verifier reported that USB had
/// been checked and the board was not in EDL. Six seconds later the gadget came
/// back as device 011. The board had been in EDL the entire time; the probe had
/// simply sampled the gap.
///
/// So a negative answer is only worth stating after watching for the whole
/// re-enumeration window. A positive answer needs no waiting at all, which is
/// why this returns the instant a gadget answers: the common case (a board that
/// really did power off) is the only one that pays the full window.
pub fn watch_for_edl(window: std::time::Duration) -> EdlProbe {
    watch_for_edl_with(scan, window, std::time::Duration::from_millis(500), |d| {
        std::thread::sleep(d)
    })
}

/// `watch_for_edl`, attributed to ONE board's hub ports.
///
/// With no ports known the answer is the bus-wide one, exactly as
/// `in_edl_on_ports` does -- and for the same reason: "some board on this bench
/// is in EDL" must not decide what happens to THIS board. Measured risk: an
/// `off` verifier that asked the whole bus would have taken any bench-mate's
/// download mode as its own and escalated to reset-then-off against a board
/// that had simply powered down.
pub fn watch_for_edl_on_ports(window: std::time::Duration, ports: &[String]) -> EdlProbe {
    watch_for_edl_on_ports_with(
        scan,
        window,
        std::time::Duration::from_millis(500),
        std::thread::sleep,
        ports,
    )
}

/// `watch_for_edl` with the bus and the clock injected, so the timing behaviour
/// is testable without hardware.
pub fn watch_for_edl_with(
    scan: impl FnMut() -> Vec<UsbDevice>,
    window: std::time::Duration,
    step: std::time::Duration,
    sleep: impl FnMut(std::time::Duration),
) -> EdlProbe {
    watch_for_edl_on_ports_with(scan, window, step, sleep, &[])
}

/// The general form: `ports` empty means the whole bus.
pub fn watch_for_edl_on_ports_with(
    mut scan: impl FnMut() -> Vec<UsbDevice>,
    window: std::time::Duration,
    step: std::time::Duration,
    mut sleep: impl FnMut(std::time::Duration),
    ports: &[String],
) -> EdlProbe {
    let window_ms = window.as_millis() as i64;
    let step_ms = step.as_millis().max(1) as i64;
    let mut waited_ms = 0i64;
    loop {
        // THE SCAN COSTS REAL TIME, AND THE WINDOW IS WALL-CLOCK.
        //
        // `scan()` is a libusb descriptor probe of every device on the bus, and
        // on a populated host it takes SECONDS (measured 6.6 s on bravo). This loop
        // used to advance `waited_ms` only by the sleep `step`, so a scan that
        // took 6.6 s counted as 0.5 s -- an 8 s window then ran ~16 iterations,
        // ~113 s of wall clock, and turned a `power off verify=poke` into a
        // two-minute wait (report #20). Counting the scan's own elapsed time
        // makes the window mean what it says. In tests `scan` is an instant
        // closure, so this adds ~0 and the injected-sleep clock still drives the
        // model exactly as before.
        let scan_start = std::time::Instant::now();
        let devices = scan();
        waited_ms += scan_start.elapsed().as_millis() as i64;
        if in_edl_on_ports(&devices, ports) {
            return EdlProbe {
                in_edl: true,
                waited_ms,
                settled: false,
                stale_qdl: 0,
            };
        }
        if waited_ms >= window_ms {
            return EdlProbe {
                in_edl: false,
                waited_ms,
                settled: true,
                // Report what IS on the bus, not merely what is not.
                stale_qdl: devices
                    .iter()
                    .filter(|d| d.is_qdl() && d.is_zombie())
                    .count(),
            };
        }
        let this = step_ms.min(window_ms - waited_ms).max(1);
        sleep(std::time::Duration::from_millis(this as u64));
        waited_ms += this;
    }
}

/// Is this device inside one of the given port paths?
///
/// §L6. Prefix by SEGMENT, so `2-3.1` covers the hub at `2-3.1` and everything
/// under it (`2-3.1.2`) while never matching the sibling `2-3.11`. A board is a
/// subtree -- its console cable, its controller and its gadget hang off the same
/// hub -- so a subtree is the unit of attribution.
pub fn on_ports(dev: &UsbDevice, ports: &[String]) -> bool {
    let Some(path) = dev.port_path.as_deref() else {
        return false;
    };
    ports.iter().any(|p| {
        path == p
            || path
                .strip_prefix(p.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// Devices that are listed but dead, and so should be cleared.
pub fn zombies(devices: &[UsbDevice]) -> Vec<&UsbDevice> {
    devices.iter().filter(|d| d.is_zombie()).collect()
}

/// Does the device answer a real request, or is it only cached?
///
/// GET_DESCRIPTOR(device) is the cheapest question that must reach the hardware.
/// A stale entry fails it -- that is precisely the "cannot read device status,
/// Resource temporarily unavailable" an operator sees from `lsusb -v` on a board
/// whose Type-C controller never signalled detach.
#[cfg(target_os = "linux")]
fn device_answers(handle: &nusb::Device) -> bool {
    use nusb::transfer::{Control, ControlType, Recipient};
    let mut buf = [0u8; 18];
    handle
        .control_in_blocking(
            Control {
                control_type: ControlType::Standard,
                recipient: Recipient::Device,
                request: 0x06, // GET_DESCRIPTOR
                value: 0x0100, // DEVICE descriptor, index 0
                index: 0,
            },
            &mut buf,
            std::time::Duration::from_millis(300),
        )
        .map(|n| n >= 8)
        .unwrap_or(false)
}

/// Enumerate USB, probing each device to see whether it still answers.
///
/// The probe is a descriptor read: cheap, read-only, and exactly the operation
/// that fails on a zombie while succeeding on anything real.
#[cfg(target_os = "linux")]
pub fn scan() -> Vec<UsbDevice> {
    if let Some(fixture) = fixture_scan() {
        return fixture;
    }
    let Ok(list) = nusb::list_devices() else {
        return Vec::new();
    };
    list.map(|d| {
        // OPENING IS NOT ENOUGH. A zombie opens fine -- the kernel still holds
        // its cached descriptors -- so `open().is_ok()` reported a dead
        // 18d1:d002 as responsive and `usb_zombies` stayed 0 while lsusb showed
        // the ghost. Liveness means the DEVICE answers, which requires actually
        // talking to it: a GET_DESCRIPTOR control transfer, the same request
        // that fails "Resource temporarily unavailable" on a stale entry.
        //
        // AN OPEN FAILURE IS NOT A DEATH CERTIFICATE. `open().ok()...
        // unwrap_or(false)` used to fold every possible error into "dead", so a
        // container without permission on /dev/bus/usb would have reported the
        // entire bench as ghosts. Only ENODEV -- usbfs has the node but the
        // device behind it is gone -- is evidence of death; EACCES and a missing
        // node mean we could not ask, which is its own answer.
        let liveness = match d.open() {
            Ok(h) => {
                if device_answers(&h) {
                    Liveness::Alive
                } else {
                    // We held it open and it did not answer: that is the zombie.
                    Liveness::Dead
                }
            }
            Err(e) => match e.raw_os_error() {
                Some(libc::ENODEV) => Liveness::Dead,
                _ => Liveness::Unknown,
            },
        };
        UsbDevice {
            vendor_id: d.vendor_id(),
            product_id: d.product_id(),
            bus: d.bus_number(),
            address: d.device_address(),
            port_path: d
                .sysfs_path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            liveness,
        }
    })
    .collect()
}

#[cfg(not(target_os = "linux"))]
pub fn scan() -> Vec<UsbDevice> {
    fixture_scan().unwrap_or_default()
}

/// A bus described by a file instead of the host: `CONMINER_USB_FIXTURE=<path>`.
///
/// The file is a JSON array of `{vendor_id, product_id, bus, address,
/// port_path, live}` (ids as numbers, `live` a bool). This is how the EDL-driven
/// paths -- the escalation an `off` takes when a QDL gadget is answering, and
/// its attribution to ONE board's ports -- get exercised without a board wedged
/// in download mode on the developer's desk. Unset, or unreadable, this is
/// never consulted; a lab host never sets it. The fixture is the whole bus, so
/// a test that wants "another board is in EDL, this one is not" writes exactly
/// that, port paths included.
pub fn fixture_scan() -> Option<Vec<UsbDevice>> {
    let path = std::env::var_os("CONMINER_USB_FIXTURE")?;
    let text = std::fs::read_to_string(&path).ok()?;
    let rows: Vec<serde_json::Value> = serde_json::from_str(&text).ok()?;
    Some(
        rows.iter()
            .map(|r| UsbDevice {
                vendor_id: r["vendor_id"].as_u64().unwrap_or(0) as u16,
                product_id: r["product_id"].as_u64().unwrap_or(0) as u16,
                bus: r["bus"].as_u64().unwrap_or(0) as u8,
                address: r["address"].as_u64().unwrap_or(0) as u8,
                port_path: r["port_path"].as_str().map(str::to_string),
                liveness: if r["live"].as_bool().unwrap_or(true) {
                    Liveness::Alive
                } else {
                    Liveness::Dead
                },
            })
            .collect(),
    )
}

/// Try to clear a zombie by resetting its port.
///
/// MEASURED LIMIT, so nobody spends another evening on it: on the ADP's dead
/// `18d1:d002`, a USBDEVFS reset fails (a device that cannot answer a descriptor
/// read cannot answer a reset either) AND the kernel's own logical disconnect --
/// `echo 1 > /sys/bus/usb/devices/<id>/remove` -- is accepted and changes
/// nothing: the entry is still in `lsusb` afterwards, because that board's
/// Type-C controller never signals detach and the hub keeps re-presenting the
/// device. Userspace has no further move. Hub port power (uhubctl) or a replug
/// are the remedies, and saying so is more use than another failed attempt.
///
/// Best effort by design: a device this dead may not answer a reset either, and
/// the honest outcomes are "cleared" or "still there", never a silent claim of
/// success. A caller that still sees it should power-cycle the hub port
/// (`uhubctl`) or replug -- which is what a human had to do before this existed.
#[cfg(target_os = "linux")]
pub fn clear_zombie(dev: &UsbDevice) -> bool {
    let Ok(list) = nusb::list_devices() else {
        return false;
    };
    for info in list {
        if info.bus_number() != dev.bus || info.device_address() != dev.address {
            continue;
        }
        // Opening a zombie usually fails outright; when it does not, a port
        // reset is the strongest thing available from userspace without
        // touching the hub's power.
        if let Ok(handle) = info.open() {
            return handle.reset().is_ok();
        }
        return false;
    }
    // Gone from the list entirely: that is the outcome we wanted.
    true
}

#[cfg(not(target_os = "linux"))]
pub fn clear_zombie(_dev: &UsbDevice) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{recovery_present_in, GadgetSig};

    #[test]
    fn a_gadget_signature_parses_exact_and_wildcard_forms() {
        assert_eq!(
            GadgetSig::parse("05c6:9008"),
            Some(GadgetSig {
                vid: 0x05c6,
                pid: Some(0x9008)
            })
        );
        assert_eq!(
            GadgetSig::parse("0x05c6:0x9008"),
            Some(GadgetSig {
                vid: 0x05c6,
                pid: Some(0x9008)
            })
        );
        // A whole-vendor wildcard, for a flasher family with shifting product ids.
        assert_eq!(
            GadgetSig::parse("18d1:*"),
            Some(GadgetSig {
                vid: 0x18d1,
                pid: None
            })
        );
        // Malformed is dropped, never matched by accident.
        assert_eq!(GadgetSig::parse("nonsense"), None);
        assert_eq!(GadgetSig::parse("05c6"), None);
        assert_eq!(GadgetSig::parse("05c6:zz"), None);
    }

    #[test]
    fn a_wildcard_signature_matches_any_product_from_the_vendor() {
        let sig = GadgetSig {
            vid: 0x18d1,
            pid: None,
        };
        assert!(sig.matches(0x18d1, 0xd00d));
        assert!(sig.matches(0x18d1, 0x4ee0));
        assert!(!sig.matches(0x05c6, 0x9008));
    }

    /// The generic recovery check is scoped to the board's OWN ports and matches
    /// a SIGNATURE LIST -- so fastboot is caught by the same path as EDL, and a
    /// neighbouring board's flasher never answers for this one.
    #[test]
    fn recovery_is_this_boards_flasher_by_signature_not_any_gadget() {
        let qdl = GadgetSig {
            vid: 0x05c6,
            pid: Some(0x9008),
        };
        let fastboot = GadgetSig {
            vid: 0x18d1,
            pid: Some(0xd00d),
        };
        let sigs = [qdl, fastboot];

        // EDL gadget on THIS board's port.
        let bus = [(0x05c6u16, 0x9008u16, Some("3-1.2".to_string()))];
        assert!(recovery_present_in(&bus, &["3-1".to_string()], &sigs));

        // The SAME shape catches fastboot -- the whole point of the signature list.
        let bus = [(0x18d1u16, 0xd00du16, Some("3-1.2".to_string()))];
        assert!(recovery_present_in(&bus, &["3-1".to_string()], &sigs));

        // A neighbour board's flasher, on its own port, must NOT answer here.
        let bus = [(0x05c6u16, 0x9008u16, Some("3-4.1".to_string()))];
        assert!(!recovery_present_in(&bus, &["3-1".to_string()], &sigs));

        // A normal (non-flasher) device on this board's port is not recovery.
        let bus = [(0x0403u16, 0x6001u16, Some("3-1.2".to_string()))];
        assert!(!recovery_present_in(&bus, &["3-1".to_string()], &sigs));

        // No ports known, or no signatures: never a false positive.
        assert!(!recovery_present_in(&bus, &[], &sigs));
        assert!(!recovery_present_in(&bus, &["3-1".to_string()], &[]));
    }

    /// A DOWNLOAD GADGET BELONGS TO A PORT, NOT TO THE BENCH.
    ///
    /// §L6 scoped zombie attribution and left EDL bus-wide. On a host with two
    /// boards that means either one sitting in download mode answers for both --
    /// and the answer decides whether a flash goes ahead. Flashing the wrong
    /// board is not a recoverable mistake.
    #[test]
    fn edl_is_this_boards_gadget_not_any_gadget_on_the_host() {
        let qdl = |port: &str| super::UsbDevice {
            vendor_id: 0x05c6,
            product_id: 0x9008,
            bus: 3,
            address: 7,
            port_path: Some(port.to_string()),
            liveness: super::Liveness::Alive,
        };
        let mine = qdl("3-1.2");
        let neighbour = qdl("3-4.1");
        let bus = vec![neighbour.clone()];
        // Bus-wide, the neighbour's gadget says "someone is in EDL" -- true, and
        // not the question that was asked.
        assert!(super::in_edl(&bus));
        assert!(
            !super::in_edl_on_ports(&bus, &["3-1".to_string()]),
            "a neighbouring board's download gadget must not answer for this one"
        );
        // This board's own gadget, on its own port, does.
        let bus = vec![neighbour, mine];
        assert!(super::in_edl_on_ports(&bus, &["3-1".to_string()]));
        // …and a child port of a declared hub still counts as this board's.
        assert!(super::in_edl_on_ports(&bus, &["3-1.2".to_string()]));
    }

    /// With no ports known, the bus-wide answer is what there is.
    ///
    /// Returning `false` would be worse than imprecise: a board really in EDL
    /// would read as not, and the caller would power-cycle it looking for a
    /// download mode it was already in. The caller reports the attribution as
    /// unknown instead of implying it was checked.
    #[test]
    fn without_declared_ports_edl_falls_back_to_the_whole_host() {
        let bus = vec![super::UsbDevice {
            vendor_id: 0x05c6,
            product_id: 0x9008,
            bus: 3,
            address: 7,
            port_path: Some("3-4.1".into()),
            liveness: super::Liveness::Alive,
        }];
        assert!(super::in_edl_on_ports(&bus, &[]));
    }

    use super::*;

    fn dev(pid: u16, responsive: bool) -> UsbDevice {
        UsbDevice {
            vendor_id: QCOM_VID,
            product_id: pid,
            bus: 3,
            address: 113,
            port_path: None,
            liveness: if responsive {
                Liveness::Alive
            } else {
                Liveness::Dead
            },
        }
    }

    #[test]
    fn a_live_qdl_gadget_means_the_board_is_in_edl() {
        assert!(in_edl(&[dev(0x9008, true)]));
        // The other PBL product ids count too.
        assert!(in_edl(&[dev(0x900e, true)]));
        assert!(in_edl(&[dev(0x9025, true)]));
    }

    /// The whole point of the liveness half. A stale 9008 left by an earlier
    /// session would otherwise report "in EDL" for a board that is powered off,
    /// and the caller's next move -- flash it -- would target nothing.
    #[test]
    fn a_stale_qdl_entry_does_not_count_as_edl() {
        assert!(
            !in_edl(&[dev(0x9008, false)]),
            "a device that no longer answers is not evidence of anything"
        );
    }

    #[test]
    fn unrelated_devices_are_not_edl() {
        let ftdi = UsbDevice {
            vendor_id: 0x0403,
            product_id: 0x6011,
            bus: 3,
            address: 5,
            port_path: None,
            liveness: Liveness::Alive,
        };
        assert!(!in_edl(&[ftdi]));
    }

    /// Measured on the rig: an ADB gadget (18d1:d002) stayed listed for minutes
    /// after the board was provably off, with descriptor reads failing.
    #[test]
    fn a_listed_but_unresponsive_device_is_a_zombie() {
        let ghost = UsbDevice {
            vendor_id: 0x18d1,
            product_id: 0xd002,
            bus: 3,
            address: 114,
            port_path: None,
            liveness: Liveness::Dead,
        };
        let live = UsbDevice {
            port_path: None,
            liveness: Liveness::Alive,
            ..ghost.clone()
        };
        assert_eq!(zombies(&[ghost.clone(), live]).len(), 1);
        assert!(ghost.is_zombie());
    }

    #[test]
    fn nothing_plugged_in_is_not_an_error() {
        assert!(!in_edl(&[]));
        assert!(zombies(&[]).is_empty());
    }

    /// "I could not ask" is not "it is dead".
    ///
    /// The probe opens a usbfs node, and that open can fail for reasons that say
    /// nothing whatever about the hardware: no permission, no node, no usbfs in
    /// the container. Folding those into "dead" would report a whole healthy
    /// bench as ghosts and then advise power-cycling a hub to fix a chmod.
    ///
    /// This is exactly the mistake `lsusb -v` invites -- without root it prints
    /// "cannot read device status", the same words a real zombie produces -- and
    /// reading that output as death cost an evening on a board that was fine.
    #[test]
    fn a_device_we_were_not_allowed_to_probe_is_unknown_not_dead() {
        let unaskable = UsbDevice {
            vendor_id: 0x18d1,
            product_id: 0xd002,
            bus: 3,
            address: 35,
            port_path: None,
            liveness: Liveness::Unknown,
        };
        assert!(
            !unaskable.is_zombie(),
            "an unprobeable device must not be reported as a zombie: the remedy \
             for a permissions problem is not a replug"
        );
        assert!(unaskable.undetermined());
        assert!(
            zombies(std::slice::from_ref(&unaskable)).is_empty(),
            "and it must not be offered up for clearing"
        );
    }

    /// The same distinction, where it decides whether a flash targets anything.
    #[test]
    fn an_unprobeable_qdl_gadget_is_neither_edl_nor_a_stale_bus() {
        let qdl = UsbDevice {
            vendor_id: QCOM_VID,
            product_id: 0x9008,
            bus: 3,
            address: 11,
            port_path: None,
            liveness: Liveness::Unknown,
        };
        assert!(
            !in_edl(std::slice::from_ref(&qdl)),
            "we never saw it answer, so claiming EDL would send a flash at a \
             device we cannot talk to"
        );
        assert_eq!(
            undetermined_qdl(std::slice::from_ref(&qdl)),
            1,
            "but it is reported as undetermined, because silently counting it \
             as a clean bus is how the caller learns nothing"
        );
        assert!(
            zombies(std::slice::from_ref(&qdl)).is_empty(),
            "and it is not evidence of a dirty bus either"
        );
    }

    /// Guards the shape of the probe itself, not just its outputs.
    #[test]
    fn an_open_failure_is_not_collapsed_into_death() {
        // Scoped to `scan` and with the needle assembled at runtime, because a
        // whole-file search for a literal MATCHES THIS TEST'S OWN TEXT and
        // passes or fails on itself. (Which it duly did, first time out.)
        let src = include_str!("usb.rs");
        let scan = src
            .split("pub fn scan() -> Vec<UsbDevice> {")
            .nth(1)
            .expect("scan must exist");
        let scan = &scan[..scan.find("\n}").unwrap_or(scan.len())];
        // Comments stripped: the comment above `scan` QUOTES the old expression
        // to explain why it was wrong, and a gate that reads prose fails on the
        // documentation of the very bug it guards. Look at the code.
        let code: String = scan
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        let collapse = format!("unwrap_or({})", "false");
        assert!(
            !code.contains(&collapse),
            "folding every open error into one boolean makes 'dead' mean \
             'could not ask', and the permission errors say nothing about the \
             device"
        );
        assert!(
            code.contains("ENODEV") && code.contains("Liveness::Dead"),
            "only ENODEV -- usbfs has the node, the device behind it is gone -- \
             is evidence of death"
        );
        assert!(
            code.contains("Liveness::Unknown"),
            "and everything else must land in Unknown"
        );
    }
}
