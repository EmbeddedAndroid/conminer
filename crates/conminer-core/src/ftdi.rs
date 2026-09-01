//! FTDI CBUS bit-bang, natively.
//!
//! Bughopper-class controllers wire a board's power, reset and EDL lines to an
//! FTDI's CBUS pins and drive them by bit-bang. That is *one* USB vendor
//! control transfer — `SET_BITMODE` — so this needs neither an FTDI library nor
//! Python; `nusb` is pure Rust, which keeps libusb out of the runtime image too.
//!
//! The part that matters operationally: claiming the interface **detaches the
//! kernel's `ftdi_sio` driver**, and the console node under
//! `/dev/serial/by-id` disappears with it. Measured on alpha:
//!
//! ```text
//! usb 2-3.2.2: usbfs: interface 0 claimed by ftdi_sio while 'python3' sets config #1
//! ftdi_sio ttyUSB0: FTDI USB Serial Device converter now disconnected from ttyUSB0
//! ```
//!
//! A power button that silently kills the console beside it is not a power
//! button. The claim uses the kernel's `USBDEVFS_DISCONNECT_CLAIM`, so the
//! kernel itself rebinds `ftdi_sio` when the file descriptor closes — on every
//! path, including a panic *and* a SIGKILL. That is a stronger guarantee than
//! any userspace cleanup, which is precisely what a timed-out hook never gets
//! to run.

use crate::error::{ErrorCode, Result, ToolError};
use std::time::Duration;

/// FTDI vendor request: set bit-bang mode.
const SIO_SET_BITMODE: u8 = 0x0B;
/// Read the current pin states (FTDI `SIO_READ_PINS`).
const SIO_READ_PINS: u8 = 0x0C;
/// Reset the chip / purge its buffers. wValue 0 = reset, 1 = purge RX, 2 = purge TX.
const SIO_RESET: u8 = 0x00;
/// Line properties (bits/parity/stop).
const SIO_SET_DATA: u8 = 0x04;
/// 8 data bits, no parity, 1 stop bit.
const LINE_8N1: u16 = 0x0008;
/// `wValue` high byte for CBUS bit-bang.
const BITMODE_CBUS: u16 = 0x20;
/// `wValue` high byte for "reset", i.e. leave bit-bang.
const BITMODE_RESET: u16 = 0x00;

const VENDOR_FTDI: u16 = 0x0403;

/// Bit layout, from the HIL's `cbus.py` — bits 4-7 are CBUS *direction*
/// (1 = output), bits 0-3 are the *value* when driven as an output. FTDI CBUS
/// HIGH turns the MOSFET on, which asserts the board's active-low `_N` line.
/// These constants are wiring, not preference: do not "tidy" them.
pub mod mask {
    /// CBUS0,1,2 outputs, all deasserted.
    pub const ALL_LOW: u8 = 0b0111_0000;
    /// CBUS2 -> PM_RESIN_N (the power button).
    pub const RESET_ASSERT: u8 = 0b0111_0100;
    /// CBUS0 -> FORCED_USB_BOOT_N (EDL).
    pub const EDL_HOLD: u8 = 0b0111_0001;
    /// CBUS1 -> MPU reset.
    pub const MPU_RESET: u8 = 0b0111_0010;
    /// Release of [`MPU_RESET`].
    pub const MPU_DONE: u8 = 0b0101_0000;
}

/// An open FTDI interface in CBUS bit-bang mode.
pub struct Cbus {
    iface: nusb::Interface,
    /// Whether we took the interface from the kernel and therefore owe it back.
    /// Whether we detached the kernel driver to take the interface. Kept for
    /// diagnostics: it is the difference between "we took it" and "nothing had
    /// it", which matters when a CBUS write silently does nothing.
    #[allow(dead_code)]
    detached: bool,
}

impl Cbus {
    /// Open the FTDI identified by `serial`, or by a device path that contains
    /// it, or the only one present.
    ///
    /// A bench usually has more than one FTDI -- this one has two, the IQ10's
    /// FT4232 and a Bughopper -- so guessing would mean power-cycling the wrong
    /// board. `hint` is the console's by-id name, which embeds the USB serial
    /// (`usb-Arduino_Bughopper_DK0HDSRI-if00-port0`), so matching a device whose
    /// `serial_number()` appears in it needs no parsing rules and cannot pick a
    /// different board by accident.
    pub fn open_for(serial: &str, hint: &str) -> Result<Self> {
        if !serial.is_empty() {
            return Self::open(serial);
        }
        if !hint.is_empty() {
            let found: Vec<String> = nusb::list_devices()
                .map_err(|e| usb_err(format!("cannot enumerate USB: {e}")))?
                .filter(|d| d.vendor_id() == VENDOR_FTDI)
                .filter_map(|d| d.serial_number().map(str::to_string))
                .filter(|s| !s.is_empty() && hint.contains(s.as_str()))
                .collect();
            if found.len() == 1 {
                return Self::open(&found[0]);
            }
        }
        Self::open(serial)
    }

    /// Open the FTDI whose serial number matches `serial`, or the only one when
    /// `serial` is empty.
    pub fn open(serial: &str) -> Result<Self> {
        let mut candidates: Vec<nusb::DeviceInfo> = nusb::list_devices()
            .map_err(|e| usb_err(format!("cannot enumerate USB: {e}")))?
            .filter(|d| d.vendor_id() == VENDOR_FTDI)
            .collect();

        if !serial.is_empty() {
            candidates.retain(|d| d.serial_number() == Some(serial));
        }
        let info = match candidates.len() {
            0 => {
                return Err(usb_err(if serial.is_empty() {
                    "no FTDI device found".to_string()
                } else {
                    format!("no FTDI device with serial {serial:?}")
                })
                .with_hint("is the controller plugged in? check `lsusb -d 0403:`"))
            }
            1 => candidates.remove(0),
            n => {
                return Err(usb_err(format!("{n} FTDI devices match")).with_hint(
                    "pass --serial, or --device <by-id path> so the serial can be resolved: \
                     guessing would drive another board's power lines",
                ))
            }
        };

        let device = info
            .open()
            .map_err(|e| usb_err(format!("cannot open FTDI: {e}")))?;

        // Interface 0 carries CBUS on these parts.
        //
        // Claim exactly ONCE. An earlier version called
        // `detach_and_claim_interface(0)` for its boolean and dropped the
        // returned handle, which released the interface and let the kernel begin
        // rebinding ftdi_sio -- then claimed again, racing that rebind. It hung
        // until the hook timed out at 30s.
        let iface = device
            .detach_and_claim_interface(0)
            .or_else(|_| device.claim_interface(0))
            .map_err(|e| usb_err(format!("cannot claim interface 0: {e}")))?;

        let cbus = Self {
            iface,
            detached: true,
        };

        // Configure the chip before driving CBUS, the way pyftdi's open path
        // does (SIO_RESET via open_from_url, then line properties, then a purge).
        // conminer previously issued SET_BITMODE on a bare claimed interface.
        // Bit-bang worked -- `reset` and `EDL` both act on the board -- but a
        // sustained hold did not: a 6s power-button press landed as a short
        // press and the board restarted instead of powering down. An
        // unconfigured chip glitching the line is the leading explanation, and
        // this is the cheap half of testing it.
        cbus.ctrl(SIO_RESET, 0)?; // reset
        cbus.ctrl(SIO_RESET, 1)?; // purge RX
        cbus.ctrl(SIO_RESET, 2)?; // purge TX
        cbus.ctrl(SIO_SET_DATA, LINE_8N1)?;

        Ok(cbus)
    }

    /// Drive the CBUS pins.
    pub fn set(&self, bits: u8) -> Result<()> {
        self.bitmode(bits, BITMODE_CBUS)
    }

    /// Hold `bits` for `dur`, then return to `release`.
    pub fn hold(&self, bits: u8, dur: Duration, release: u8) -> Result<()> {
        self.set(bits)?;
        std::thread::sleep(dur);
        self.set(release)
    }

    /// One FTDI vendor control-out with no data payload.
    fn ctrl(&self, request: u8, value: u16) -> Result<()> {
        use nusb::transfer::{Control, ControlType, Recipient};
        self.iface
            .control_out_blocking(
                Control {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request,
                    value,
                    index: 1,
                },
                &[],
                Duration::from_secs(2),
            )
            .map_err(|e| {
                usb_err(format!(
                    "FTDI request {request:#04x}({value:#06x}) failed: {e}"
                ))
            })?;
        Ok(())
    }

    /// Read the CBUS pins back.
    ///
    /// IMPORTANT: on this controller the CBUS lines are OUTPUTS driving the
    /// board's power button, so this reports what conminer last COMMANDED, not
    /// what the board is doing. That distinction is the whole PWR_OFF versus
    /// MD_PS_HOLD lesson from the Bantam boards: the request and the truth
    /// disagree exactly when something interesting has gone wrong. Callers must
    /// label this "commanded", never present it as a measurement.
    pub fn read_pins(&self) -> Result<u8> {
        use nusb::transfer::{Control, ControlType, Recipient};
        let mut buf = [0u8; 1];
        let n = self
            .iface
            .control_in_blocking(
                Control {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: SIO_READ_PINS,
                    value: 0,
                    index: 1, // interface A
                },
                &mut buf,
                Duration::from_secs(2),
            )
            .map_err(|e| usb_err(format!("READ_PINS failed: {e}")))?;
        if n == 0 {
            return Err(usb_err("READ_PINS returned no data".to_string()));
        }
        Ok(buf[0])
    }

    fn bitmode(&self, bits: u8, mode: u16) -> Result<()> {
        use nusb::transfer::{Control, ControlType, Recipient};
        let value = (mode << 8) | bits as u16;
        self.iface
            .control_out_blocking(
                Control {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: SIO_SET_BITMODE,
                    value,
                    index: 1, // interface A
                },
                &[],
                Duration::from_secs(2),
            )
            .map_err(|e| usb_err(format!("SET_BITMODE({bits:#010b}) failed: {e}")))?;
        Ok(())
    }
}

impl Drop for Cbus {
    fn drop(&mut self) {
        // Park the lines deasserted and leave bit-bang. Best-effort: a failure
        // here must not mask the outcome of the action the caller asked for.
        let _ = self.bitmode(mask::ALL_LOW, BITMODE_CBUS);
        let _ = self.bitmode(0, BITMODE_RESET);
    }
}

/// Give `ftdi_sio` back every FTDI interface that currently has no driver.
///
/// This is not optional bookkeeping. Claiming an FTDI detaches `ftdi_sio`, and
/// the board's console node under `/dev/serial/by-id` goes with it. An earlier
/// version of this code assumed the kernel rebinds implicitly when the usbfs
/// file descriptor closes; measured on hardware it does NOT, and the console
/// stayed missing across every subsequent action. pyftdi does not rely on that
/// either -- pyusb's `dispose_resources()` calls `libusb_attach_kernel_driver`
/// explicitly, which is why the HIL never saw this. So do it explicitly too.
///
/// Deliberately a targeted driver bind, never a USB port or hub reset: a
/// malformed reset on this bench deauthorized a controller and needed manual
/// recovery.
pub fn rebind_ftdi_sio() -> usize {
    let mut bound = 0;
    let mut failed: Vec<String> = Vec::new();
    let Ok(devices) = nusb::list_devices() else {
        tracing::warn!("cannot enumerate USB to reattach ftdi_sio");
        return 0;
    };
    for d in devices.filter(|d| d.vendor_id() == VENDOR_FTDI) {
        let (bus, addr) = (d.bus_number(), d.device_address());
        // Only interfaces the kernel is not already driving. Skipping the rest
        // keeps the common case silent instead of logging every tick.
        if !interface_unbound(bus, addr) {
            continue;
        }
        if reattach_interface(bus, addr, 0) {
            tracing::info!(bus, addr, "reattached ftdi_sio; console restored");
            bound += 1;
        } else {
            failed.push(format!("{bus:03}/{addr:03}"));
        }
    }
    if !failed.is_empty() {
        // Say so. An earlier version logged only successes, so a total failure
        // produced NO output at all -- the console stayed missing after a power
        // off and nothing in the log said why.
        tracing::warn!(
            devices = ?failed,
            "could not reattach ftdi_sio; a console will stay missing. \
             Is /dev/bus/usb/<bus>/<addr> visible in this container? A device that \
             re-enumerated after container start gets a NEW address, and a stale \
             /dev mount will not have it."
        );
    }
    bound
}

/// Is this device's interface 0 currently without a driver?
///
/// Read from sysfs, which is visible even where usbfs nodes are not, so the
/// answer is right even when the reattach itself cannot proceed.
fn interface_unbound(bus: u8, addr: u8) -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return true; // cannot tell; try anyway
    };
    for e in entries.flatten() {
        let p = e.path();
        let busnum = std::fs::read_to_string(p.join("busnum"))
            .ok()
            .and_then(|v| v.trim().parse::<u8>().ok());
        let devnum = std::fs::read_to_string(p.join("devnum"))
            .ok()
            .and_then(|v| v.trim().parse::<u8>().ok());
        if busnum == Some(bus) && devnum == Some(addr) {
            let name = e.file_name().to_string_lossy().into_owned();
            return !p.join(format!("{name}:1.0")).join("driver").exists();
        }
    }
    true
}

/// Ask the kernel to reattach its driver to one interface, via usbfs.
///
/// This is the ioctl libusb's `attach_kernel_driver` issues, and it is why
/// pyftdi-based tooling never loses a console: the earlier sysfs approach
/// (`/sys/bus/usb/drivers/ftdi_sio/bind`) cannot work from a container, where
/// sysfs is mounted read-only — measured, with the honest log line
/// "could not rebind ftdi_sio" on every attempt. usbfs is a character device we
/// already hold rw access to, so this succeeds where the sysfs write cannot.
///
/// Returns true only when the kernel actually reattached, so callers can report
/// what really happened rather than what was attempted.
fn reattach_interface(bus: u8, addr: u8, ifno: i32) -> bool {
    use std::os::unix::io::AsRawFd;

    #[repr(C)]
    struct UsbfsIoctl {
        ifno: libc::c_int,
        ioctl_code: libc::c_int,
        data: *mut libc::c_void,
    }
    // _IO('U', 23) — reattach the kernel driver to this interface.
    const USBDEVFS_CONNECT: libc::c_int = 0x5517;
    // _IOWR('U', 18, struct usbdevfs_ioctl)
    const USBDEVFS_IOCTL: libc::c_ulong = 0xc0105512;

    let path = format!("/dev/bus/usb/{bus:03}/{addr:03}");
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    else {
        return false;
    };
    let mut req = UsbfsIoctl {
        ifno,
        ioctl_code: USBDEVFS_CONNECT,
        data: std::ptr::null_mut(),
    };
    // SAFETY: `file` is an open usbfs node and `req` outlives the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), USBDEVFS_IOCTL, &mut req) };
    // ENODATA/EBUSY simply mean "already bound", which is success for our
    // purposes: the console is present either way.
    rc >= 0
}

fn usb_err(msg: String) -> ToolError {
    ToolError::new(ErrorCode::DeviceGone, msg)
}
