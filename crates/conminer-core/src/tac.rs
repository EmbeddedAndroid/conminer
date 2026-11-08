//! FTDI TAC ("Alpaca") board control, natively.
//!
//! A Qualcomm TAC debug board is an FT4232H whose four channels are split by an
//! EEPROM setting: some are UARTs, the rest are 8-bit GPIO ports wired to the
//! SoC's power, reset, strap and USB-disconnect lines. On the RIDE MICRO 4.0
//! that is channels A and B as consoles, C and D as GPIO — which is why the
//! host shows exactly two `ttyUSB` nodes for a four-channel chip, and why the
//! kernel log shows `ftdi_sio` being detached from the other two.
//!
//! The vendor drives this from Python (`pyftdi`'s `GpioAsyncController`) against
//! a `.tcnf` JSON file per board type. This is the same wire protocol without
//! the interpreter: async bit-bang is one vendor control transfer to set the
//! direction mask, then one bulk write per pin change.
//!
//! THREE THINGS HERE ARE NOT PREFERENCES.
//!
//! 1. **Opening must not change a pin.** The vendor tool writes every pin's
//!    `initial_value` at startup, which forces `pwr_off` low — so merely
//!    starting it powers a board that someone deliberately switched off. This
//!    seeds its shadow from `READ_PINS` instead and writes only what an action
//!    asks for.
//! 2. **Dropping must not reset the chip.** `Cbus` parks its lines and leaves
//!    bit-bang, which is right for a momentary button. Here the lines are
//!    LEVELS: `pwr_off` HIGH is what holds a board off, so leaving bit-bang
//!    would release it and boot the board back up on the way out of a
//!    `power off`. This driver deliberately leaves the chip exactly as the
//!    action left it.
//! 3. **`ftdi_sio` is not rebound afterwards.** Rebinding puts the channel back
//!    into UART mode, which drops the same levels. `ftdi::rebind_ftdi_sio` only
//!    ever touches interface 0, so a console-carrying channel is restored while
//!    a GPIO channel is left alone — verified against this board.

use crate::error::{ErrorCode, Result, ToolError};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

const VENDOR_FTDI: u16 = 0x0403;

/// FTDI vendor requests.
const SIO_SET_BITMODE: u8 = 0x0B;
const SIO_READ_PINS: u8 = 0x0C;
const SIO_RESET: u8 = 0x00;

/// `wValue` high byte: asynchronous bit-bang. In this mode the low byte is the
/// DIRECTION mask (1 = output) and the pin levels come from bulk writes — unlike
/// CBUS mode, where one request carries both.
const BITMODE_ASYNC_BITBANG: u16 = 0x01;

/// One pin: which GPIO port, and which bit of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinRef {
    pub bus: char,
    pub bit: u8,
}

impl PinRef {
    pub fn mask(&self) -> u8 {
        1 << self.bit
    }
}

/// Which vendor sequence set a board's pins are driven by.
///
/// The two TAC configs seen so far wire power differently, and their scripts
/// differ with it. A RIDE board is held off by one load-switch level
/// (`pwr_off`). An MTP on the Alpaca-lite cuts its battery and both VBUS feeds
/// to go off, and only comes up from a power-key press once they are restored.
/// Mapping one family's pin names onto the other's sequences would drive the
/// wrong lines, so the family is part of the profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TacFamily {
    /// `pwr_off` / `sw_dnld` / `ss_force_edl`: `TAC_FTDI_37.tcnf`.
    Ride,
    /// `battery` / `pkey` / `pedl` behind `TC_READY_N`: `TAC_FTDI_13.tcnf`.
    Mtp,
}

/// What a TAC config says about one board type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TacProfile {
    /// The USB product descriptor this matches, e.g. `RIDE MICRO 4.0`.
    pub usb_descriptor: String,
    pub family: TacFamily,
    /// Channels configured as GPIO rather than UART.
    pub gpio_buses: Vec<char>,
    /// Command name -> pin. Names are the vendor's (`pwr_off`, `sw_dnld`, …).
    pub pins: BTreeMap<String, PinRef>,
    /// The pin that reports whether the board is ACTUALLY powered, if any.
    ///
    /// Measured on the bravo host's IQ8, five power cycles: `md_resout` reads 1
    /// while the board is running and 0 while it is off, and it is the SoC
    /// driving it, not us. That makes it the rare thing a strap controller
    /// usually cannot offer -- a measurement rather than a memory of what was
    /// commanded. It only became readable once this driver stopped driving the
    /// pin as an output the way the vendor config does.
    pub sense: Option<String>,
    /// Pins we drive as INPUTS, against the vendor's own direction mask.
    ///
    /// `md_resout` is the SoC's RESOUT reported back to the TAC through a
    /// buffer; the vendor config declares every pin an output and drives all
    /// eight, which means the FTDI and the SoC both drive this line. Taking it
    /// as an input removes that contention and is the only pin on this board
    /// that could ever answer "is it actually on?" — nothing in the vendor's
    /// own sequences ever sets it.
    pub inputs: Vec<String>,
}

impl TacProfile {
    /// The built-in catalogue: boards conminer can drive with no files at all.
    ///
    /// Deliberately only what has been read off real hardware. A TAC config for
    /// another board is a drop-in (`from_tcnf`), not a guess made here.
    pub fn builtin() -> Vec<TacProfile> {
        vec![TacProfile::ride_micro_4_0(), TacProfile::alpaca_lite_mtp()]
    }

    /// `TAC_FTDI_13.tcnf`, transcribed: the config the vendor's pytac falls
    /// back to for any FTDI Alpaca-lite, and the one this product drives.
    /// Buses A and B are UARTs, C and D GPIO. The file's own `usb_descriptor`
    /// is empty, which is why this cannot simply be dropped in as a `.tcnf`.
    ///
    /// Levels are written exactly as the vendor script states them: pytac
    /// ignores the config's `inverted` flags, so `battery 1` drives D1 high,
    /// and that is "battery disconnected".
    fn alpaca_lite_mtp() -> TacProfile {
        let pins = [
            ("pshold", 'C', 0),
            ("uim1", 'C', 1),
            ("headset", 'C', 2),
            ("uim2", 'C', 3),
            ("sdcard", 'C', 4),
            // Secondary-SoC (Fusion) EDL and PM_RESIN_N: present on the board,
            // used by no sequence here.
            ("sedl", 'C', 5),
            ("usb1", 'C', 6),
            ("sresn", 'C', 7),
            ("volup", 'D', 0),
            ("battery", 'D', 1),
            ("voldn", 'D', 2),
            ("pkey", 'D', 3),
            ("eud", 'D', 4),
            ("pedl", 'D', 5),
            ("TC_READY_N", 'D', 6),
            ("usb0", 'D', 7),
        ];
        TacProfile {
            usb_descriptor: "ALPACA-LITE MTP DEBUG BOARD".into(),
            family: TacFamily::Mtp,
            gpio_buses: vec!['C', 'D'],
            // The config has no feedback line: power-state can only report
            // what was commanded.
            sense: None,
            pins: pins
                .into_iter()
                .map(|(n, bus, bit)| (n.to_string(), PinRef { bus, bit }))
                .collect(),
            inputs: Vec::new(),
        }
    }

    /// The pin whose level says whether power was last commanded off, and
    /// that level.
    pub fn power_off_pin(&self) -> (&'static str, bool) {
        match self.family {
            TacFamily::Ride => ("pwr_off", true),
            TacFamily::Mtp => ("battery", true),
        }
    }

    /// `TAC_FTDI_37.tcnf`, transcribed. `chip1BusSet: 12` = buses C and D.
    fn ride_micro_4_0() -> TacProfile {
        let pins = [
            // bus C — SOC1 straps and the RESOUT feedback
            ("ms_ps_hold", 'C', 0),
            ("mode0", 'C', 1),
            ("md_resout", 'C', 2),
            ("mode1", 'C', 3),
            ("ss_force_edl", 'C', 4),
            ("usb2", 'C', 5),
            ("usb1", 'C', 6),
            ("sail_ps_hold", 'C', 7),
            // bus D — power, boot select, output enable
            ("uefi", 'D', 0),
            ("pwr_off", 'D', 1),
            // D2 `pmic_resin` is `enabled: false` in the vendor config for this
            // board and is deliberately absent: a reset here is a power cycle.
            ("kpd_pwr", 'D', 3),
            ("eud_en", 'D', 4),
            ("sw_dnld", 'D', 5),
            ("oe", 'D', 6),
            ("usb0", 'D', 7),
        ];
        TacProfile {
            usb_descriptor: "RIDE MICRO 4.0".into(),
            family: TacFamily::Ride,
            gpio_buses: vec!['C', 'D'],
            sense: Some("md_resout".into()),
            pins: pins
                .into_iter()
                .map(|(n, bus, bit)| (n.to_string(), PinRef { bus, bit }))
                .collect(),
            inputs: vec!["md_resout".into()],
        }
    }

    /// Parse a vendor `.tcnf`, so a TAC board this build has never heard of
    /// works by dropping its config next to the others.
    ///
    /// Only the JSON is read. The file also carries a `script` block in a small
    /// vendor-specific language; the sequences it describes are implemented
    /// natively in [`sequence`] instead, because transpiling an untrusted script
    /// to drive power lines is a worse idea than writing the six sequences out.
    pub fn from_tcnf(json: &str) -> Result<TacProfile> {
        let v: Value = serde_json::from_str(json)
            .map_err(|e| bad_config(format!("not valid TAC config JSON: {e}")))?;
        let usb_descriptor = v
            .get("usb_descriptor")
            .and_then(Value::as_str)
            .ok_or_else(|| bad_config("TAC config has no usb_descriptor".to_string()))?
            .to_string();
        // `bus_function: 2` is GPIO; 1 is UART.
        let gpio_buses: Vec<char> = v
            .get("bus")
            .and_then(Value::as_array)
            .map(|b| {
                b.iter()
                    .filter(|e| e.get("bus_function").and_then(Value::as_i64) == Some(2))
                    .filter_map(|e| e.get("bus").and_then(Value::as_str))
                    .filter_map(|s| s.chars().next())
                    .collect()
            })
            .unwrap_or_default();
        let mut pins = BTreeMap::new();
        for p in v
            .get("pins")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if p.get("enabled").and_then(Value::as_bool) != Some(true) {
                continue;
            }
            let (Some(cmd), Some(bus), Some(bit)) = (
                p.get("command").and_then(Value::as_str),
                p.get("bus")
                    .and_then(Value::as_str)
                    .and_then(|s| s.chars().next()),
                p.get("pin_number").and_then(|n| match n {
                    Value::String(s) => s.parse::<u8>().ok(),
                    Value::Number(n) => n.as_u64().map(|v| v as u8),
                    _ => None,
                }),
            ) else {
                continue;
            };
            // Pins on a UART channel have no port to drive: the vendor's own
            // loader creates a Port only for `bus_function == 2`, so these are
            // inert placeholders for a second SoC that is not fitted.
            if !gpio_buses.contains(&bus) {
                continue;
            }
            pins.insert(cmd.to_string(), PinRef { bus, bit });
        }
        if pins.is_empty() {
            return Err(bad_config(format!(
                "TAC config for {usb_descriptor:?} has no usable pins on a GPIO bus"
            )));
        }
        let sense = pins
            .contains_key("md_resout")
            .then(|| "md_resout".to_string());
        let inputs = sense.clone().into_iter().collect();
        let family = if pins.contains_key("battery") && pins.contains_key("pkey") {
            TacFamily::Mtp
        } else {
            TacFamily::Ride
        };
        Ok(TacProfile {
            usb_descriptor,
            family,
            gpio_buses,
            sense,
            pins,
            inputs,
        })
    }

    /// The direction mask for one bus: 1 = output, per FTDI.
    pub fn direction(&self, bus: char) -> u8 {
        let mut dir = 0xFFu8;
        for name in &self.inputs {
            if let Some(p) = self.pins.get(name) {
                if p.bus == bus {
                    dir &= !p.mask();
                }
            }
        }
        dir
    }

    pub fn pin(&self, name: &str) -> Option<PinRef> {
        self.pins.get(name).copied()
    }
}

/// Where a deployment may drop vendor `.tcnf` files for TAC boards this build
/// has no pin map for.
pub fn config_dir() -> std::path::PathBuf {
    std::env::var("CONMINER_TAC_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/etc/conminer/tac.d"))
}

/// Every TAC profile this process knows: built in, plus anything dropped in.
///
/// Read once. Discovery consults it on every scan to keep a TAC's GPIO channels
/// out of the console list, so re-reading a directory per device per second
/// would be a poor trade for a file that changes when someone edits it.
pub fn catalogue() -> &'static [TacProfile] {
    static CATALOGUE: once_cell::sync::OnceCell<Vec<TacProfile>> = once_cell::sync::OnceCell::new();
    CATALOGUE.get_or_init(|| {
        let mut out = load_dir(&config_dir());
        out.extend(TacProfile::builtin());
        out
    })
}

/// Load every `.tcnf` in a directory, skipping what cannot be parsed.
///
/// A malformed file must not stop the built-in profiles from working: "one bad
/// config takes out board control for the whole bench" is a worse failure than
/// the file being skipped with a warning.
pub fn load_dir(dir: &std::path::Path) -> Vec<TacProfile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("tcnf") {
            continue;
        }
        match std::fs::read_to_string(&path).map(|s| TacProfile::from_tcnf(&s)) {
            Ok(Ok(p)) => out.push(p),
            Ok(Err(err)) => {
                tracing::warn!(file = %path.display(), error = %err.message, "ignoring TAC config")
            }
            Err(err) => {
                tracing::warn!(file = %path.display(), error = %err, "cannot read TAC config")
            }
        }
    }
    out
}

/// The GPIO channel a by-id name belongs to, if it is one.
///
/// THE CONSOLE LIST MUST NOT CONTAIN GPIO. On a freshly booted host `ftdi_sio`
/// binds all four channels of a TAC, so `/dev/serial/by-id` shows four ports and
/// two of them are the board's power and strap lines. Serving those to ser2net
/// is not merely untidy: opening a tty asserts DTR and RTS, which on a bit-bang
/// channel is a write to whatever those pins are wired to. Measured on the bravo
/// host at boot -- ttyUSB0 through ttyUSB3 all attached, and only the vendor
/// tool claiming two of them away made the list look right.
pub fn gpio_channel(by_id_name: &str) -> Option<char> {
    gpio_channel_in(by_id_name, catalogue())
}

/// The testable half: no filesystem, no statics.
pub fn gpio_channel_in(by_id_name: &str, profiles: &[TacProfile]) -> Option<char> {
    let ifnum = interface_number(by_id_name)?;
    let bus = (b'A' + ifnum) as char;
    profiles
        .iter()
        .find(|p| {
            // A by-id name carries the USB product string with spaces turned
            // into underscores: "RIDE MICRO 4.0" -> "RIDE_MICRO_4.0".
            by_id_name.contains(&p.usb_descriptor.replace(' ', "_")) && p.gpio_buses.contains(&bus)
        })
        .map(|_| bus)
}

/// `usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if02-port0` -> 2.
fn interface_number(by_id_name: &str) -> Option<u8> {
    let rest = by_id_name.rsplit_once("-if")?.1;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// What a caller asked the board to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    On,
    Off,
    /// Off, settle, on.
    Cycle,
    /// Main-domain *and* SAIL EDL: the vendor's `bootToEDL`.
    Edl,
    /// SAIL domain only: `bootToSecondaryEDL`.
    SailEdl,
    Uefi,
    Fastboot,
    /// Boot with the EUD enable line held, so the early boot loader turns on
    /// the Embedded USB Debugger itself. MTP only.
    Eud,
    /// Cut / restore the board's two USB VBUS feeds while it stays powered, so
    /// the USB controller sees a disconnect (suspend refuses with a host
    /// attached). MTP only; no boot.
    UsbOff,
    UsbOn,
    /// Release every strap without touching power.
    Clear,
}

impl Action {
    pub fn parse(s: &str) -> Option<Action> {
        Some(match s.to_ascii_uppercase().as_str() {
            "ON" => Action::On,
            "OFF" => Action::Off,
            "CYCLE" | "RESET" => Action::Cycle,
            "EDL" => Action::Edl,
            "SAIL_EDL" => Action::SailEdl,
            "UEFI" => Action::Uefi,
            "FASTBOOT" => Action::Fastboot,
            "EUD" => Action::Eud,
            "USB_OFF" => Action::UsbOff,
            "USB_ON" => Action::UsbOn,
            "CLEAR" | "NONE" | "NORMAL" => Action::Clear,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Action::On => "on",
            Action::Off => "off",
            Action::Cycle => "cycle",
            Action::Edl => "EDL",
            Action::SailEdl => "SAIL_EDL",
            Action::Uefi => "UEFI",
            Action::Fastboot => "FASTBOOT",
            Action::Eud => "EUD",
            Action::UsbOff => "USB_OFF",
            Action::UsbOn => "USB_ON",
            Action::Clear => "clear",
        }
    }
}

/// One step of a sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Set(&'static str, bool),
    Delay(u64),
}

/// The vendor's `script` block for the RIDE MICRO 4.0, natively.
///
/// TRANSCRIBED, NOT INVENTED. From `TAC_FTDI_37.tcnf`:
///
/// ```text
/// def powerOn()   pwr_off 0
/// def powerOff()  pwr_off 1
/// def reset()     powerOff; delay 1500; powerOn
/// def bootToEDL() powerOff; sw_dnld 1; ss_force_edl 1; delay 1500;
///                 powerOn;  delay 5000; sw_dnld 0; ss_force_edl 0
/// def bootToSecondaryEDL()
///                 powerOff; ss_force_edl 1; delay 1500;
///                 powerOn;  delay 5000; ss_force_edl 0
/// def bootToUEFI() reset; kpd_pwr 0; uefi 1; kpd_pwr 1; delay 8000;
///                 kpd_pwr 0; uefi 0
/// def bootToFastboot()
///                 powerOff; delay 1500; usb2 1; delay 500;
///                 powerOn;  delay 8000; usb2 0
/// ```
///
/// The strap timings are the board's, not ours: the 1500 ms before power-on is
/// the rail collapsing, and the 5000/8000 ms afterwards is the window in which
/// the SoC samples the strap. Shortening either is how EDL silently stops
/// working.
///
/// `settle_ms` replaces the vendor's fixed 1500 ms off-time so a board that
/// needs longer can be configured without editing this.
pub fn sequence(profile: &TacProfile, action: Action, settle_ms: u64) -> Result<Vec<Step>> {
    sequence_timed(profile, action, settle_ms, MTP_FASTBOOT_HOLD_MS)
}

/// How long an MTP FASTBOOT entry keeps volume down held after the power-key
/// release, unless the controller profile says otherwise.
pub const MTP_FASTBOOT_HOLD_MS: u64 = 10_000;

/// [`sequence`] with the MTP's FASTBOOT hold given explicitly. RIDE boards
/// select fastboot with a strap, not a key, and ignore it.
pub fn sequence_timed(
    profile: &TacProfile,
    action: Action,
    settle_ms: u64,
    fastboot_hold_ms: u64,
) -> Result<Vec<Step>> {
    if profile.family == TacFamily::Mtp {
        return mtp_sequence(profile, action, settle_ms, fastboot_hold_ms);
    }
    let need = |name: &'static str| -> Result<()> {
        if profile.pins.contains_key(name) {
            Ok(())
        } else {
            Err(ToolError::new(
                ErrorCode::HookNotConfigured,
                format!(
                    "this TAC board ({}) has no {name:?} pin, so it cannot do that",
                    profile.usb_descriptor
                ),
            )
            .with_hint("the board's TAC config decides which actions exist"))
        }
    };
    let steps = match action {
        Action::On => {
            need("pwr_off")?;
            vec![Step::Set("pwr_off", false)]
        }
        Action::Off => {
            need("pwr_off")?;
            vec![Step::Set("pwr_off", true)]
        }
        Action::Cycle => {
            need("pwr_off")?;
            vec![
                Step::Set("pwr_off", true),
                Step::Delay(settle_ms),
                Step::Set("pwr_off", false),
            ]
        }
        Action::Edl => {
            need("pwr_off")?;
            need("sw_dnld")?;
            need("ss_force_edl")?;
            vec![
                Step::Set("pwr_off", true),
                Step::Set("sw_dnld", true),
                Step::Set("ss_force_edl", true),
                Step::Delay(settle_ms),
                Step::Set("pwr_off", false),
                Step::Delay(5000),
                Step::Set("sw_dnld", false),
                Step::Set("ss_force_edl", false),
            ]
        }
        Action::SailEdl => {
            need("pwr_off")?;
            need("ss_force_edl")?;
            vec![
                Step::Set("pwr_off", true),
                Step::Set("ss_force_edl", true),
                Step::Delay(settle_ms),
                Step::Set("pwr_off", false),
                Step::Delay(5000),
                Step::Set("ss_force_edl", false),
            ]
        }
        Action::Uefi => {
            need("pwr_off")?;
            need("uefi")?;
            need("kpd_pwr")?;
            vec![
                // The vendor's bootToUEFI starts with a full `reset`.
                Step::Set("pwr_off", true),
                Step::Delay(settle_ms),
                Step::Set("pwr_off", false),
                Step::Set("kpd_pwr", false),
                Step::Set("uefi", true),
                Step::Set("kpd_pwr", true),
                Step::Delay(8000),
                Step::Set("kpd_pwr", false),
                Step::Set("uefi", false),
            ]
        }
        Action::Fastboot => {
            need("pwr_off")?;
            need("usb2")?;
            vec![
                Step::Set("pwr_off", true),
                Step::Delay(settle_ms),
                Step::Set("usb2", true),
                Step::Delay(500),
                Step::Set("pwr_off", false),
                Step::Delay(8000),
                Step::Set("usb2", false),
            ]
        }
        Action::Eud | Action::UsbOff | Action::UsbOn => {
            return Err(ToolError::new(
                ErrorCode::HookNotConfigured,
                format!(
                    "{} is only defined for the MTP Alpaca-lite, not {}",
                    action.as_str(),
                    profile.usb_descriptor
                ),
            ))
        }
        Action::Clear => {
            // Every strap low, power untouched. A latched strap is a trap: the
            // sequences above release their own, but an interrupted one (a hook
            // killed at its timeout, mid-hold) can leave one set.
            ["sw_dnld", "ss_force_edl", "uefi", "usb2", "kpd_pwr"]
                .into_iter()
                .filter(|n| profile.pins.contains_key(*n))
                .map(|n| Step::Set(n, false))
                .collect()
        }
    };
    Ok(steps)
}

/// The MTP Alpaca-lite's `script` block, natively.
///
/// TRANSCRIBED, NOT INVENTED. From `TAC_FTDI_13.tcnf` (timing variables at
/// their defaults: `edl` 1300, `uefi` 20000, `fastboot` 1000):
///
/// ```text
/// def powerOffTheDevice() battery 1; usb0 1; usb1 1
/// def powerOnTheDevice()  delay 500; battery 0; delay 900; usb0 0; usb1 0; delay 30
/// def powerOff()  powerOffTheDevice; pkey 0; volup 0; voldn 0; pedl 0
/// def powerOn()   powerOff; powerOnTheDevice; pkey 1; delay 800; pkey 0
/// def bootToEDL() powerOffTheDevice; pkey 0; volup 0; voldn 0; pedl 1;
///                 powerOnTheDevice; pkey 1; delay $edl; pkey 0; pedl 0
/// def bootToUEFI() powerOffTheDevice; pkey 0; volup 1; voldn 0; pedl 0;
///                 powerOnTheDevice; pkey 1; delay $uefi; pkey 0; volup 0;
///                 delay 12; pkey 1; volup 1; pkey 0; volup 0
/// def bootToFastboot() powerOffTheDevice; pkey 0; volup 0; voldn 1; pedl 0;
///                 powerOnTheDevice; pkey 1; delay 800; pkey 0;
///                 delay $fastboot; voldn 0; delay 30; voldn 1; voldn 0
/// ```
///
/// Three departures. The first two are about getting the levels onto the
/// board safely:
///
/// - `TC_READY_N` gates the board-side outputs ("SW must program it to 1 to
///   enable some output signals"). The vendor tool sets it at startup along
///   with every other initial value, which is what powers a board on the
///   moment the tool opens. Here it is set inside each sequence, right after
///   the power-off levels, so the first thing the board sees is "off".
/// - `settle_ms` is added to the vendor's 500 ms off-time, as the RIDE
///   sequences do, so a slow rail can be given longer without editing this.
///
/// The third is FASTBOOT's volume-down hold. The vendor script releases the
/// key 1.8 s after the power-key press, which suits a boot loader that reads
/// it that early. U-Boot reads it only after its storage and USB scans, about
/// 5 to 6 s after the power-key release on the board this was measured on,
/// and missed the key every time. So the key is held `fastboot_hold_ms` past
/// the release and then let go, without the vendor's closing re-press: a
/// level is all the reader needs.
///
/// `sedl` (secondary-SoC EDL) is not exposed: it is the Fusion modem's strap,
/// and EDL here means the application processor.
fn mtp_sequence(
    profile: &TacProfile,
    action: Action,
    settle_ms: u64,
    fastboot_hold_ms: u64,
) -> Result<Vec<Step>> {
    for name in [
        "battery",
        "usb0",
        "usb1",
        "pkey",
        "volup",
        "voldn",
        "pedl",
        "TC_READY_N",
    ] {
        if !profile.pins.contains_key(name) {
            return Err(ToolError::new(
                ErrorCode::HookNotConfigured,
                format!(
                    "this TAC board ({}) has no {name:?} pin, so it cannot do that",
                    profile.usb_descriptor
                ),
            )
            .with_hint("the board's TAC config decides which actions exist"));
        }
    }
    // powerOffTheDevice, then the output enable.
    let off = || {
        vec![
            Step::Set("battery", true),
            Step::Set("usb0", true),
            Step::Set("usb1", true),
            Step::Set("TC_READY_N", true),
        ]
    };
    // The EUD line is only ever held for a boot asked for with `EUD`; every
    // other power-on releases it, so it never leaks into a normal boot.
    let has_eud = profile.pins.contains_key("eud");
    let buttons = |volup: bool, voldn: bool, pedl: bool| {
        let mut v = vec![
            Step::Set("pkey", false),
            Step::Set("volup", volup),
            Step::Set("voldn", voldn),
            Step::Set("pedl", pedl),
        ];
        if has_eud {
            v.push(Step::Set("eud", false));
        }
        v
    };
    let on = || {
        vec![
            Step::Delay(500 + settle_ms),
            Step::Set("battery", false),
            Step::Delay(900),
            Step::Set("usb0", false),
            Step::Set("usb1", false),
            Step::Delay(30),
        ]
    };
    let press = |hold_ms: u64| {
        vec![
            Step::Set("pkey", true),
            Step::Delay(hold_ms),
            Step::Set("pkey", false),
        ]
    };
    let steps: Vec<Step> = match action {
        Action::Off => [off(), buttons(false, false, false)].concat(),
        Action::On | Action::Cycle => {
            [off(), buttons(false, false, false), on(), press(800)].concat()
        }
        Action::Edl => [
            off(),
            buttons(false, false, true),
            on(),
            press(1300),
            vec![Step::Set("pedl", false)],
        ]
        .concat(),
        Action::Uefi => [
            off(),
            buttons(true, false, false),
            on(),
            vec![
                Step::Set("pkey", true),
                Step::Delay(20000),
                Step::Set("pkey", false),
                Step::Set("volup", false),
                Step::Delay(12),
                Step::Set("pkey", true),
                Step::Set("volup", true),
                Step::Set("pkey", false),
                Step::Set("volup", false),
            ],
        ]
        .concat(),
        Action::Fastboot => [
            off(),
            buttons(false, true, false),
            on(),
            press(800),
            vec![Step::Delay(fastboot_hold_ms), Step::Set("voldn", false)],
        ]
        .concat(),
        Action::SailEdl => {
            return Err(ToolError::new(
                ErrorCode::HookNotConfigured,
                format!(
                    "this TAC board ({}) has no SAIL domain; its secondary EDL strap is \
                     the Fusion modem's",
                    profile.usb_descriptor
                ),
            ))
        }
        Action::Eud => {
            if !has_eud {
                return Err(ToolError::new(
                    ErrorCode::HookNotConfigured,
                    format!(
                        "this TAC board ({}) has no \"eud\" pin",
                        profile.usb_descriptor
                    ),
                ));
            }
            // Not in the vendor script, which never drives this pin. Held from
            // before power-on and left held: the loader samples it during
            // boot, and `clear` or the next power action releases it.
            [
                off(),
                buttons(false, false, false),
                vec![Step::Set("eud", true)],
                on(),
                press(800),
            ]
            .concat()
        }
        // Only the VBUS feeds move: `battery` keeps the board running.
        Action::UsbOff => vec![
            Step::Set("usb0", true),
            Step::Set("usb1", true),
            Step::Set("TC_READY_N", true),
        ],
        Action::UsbOn => vec![
            Step::Set("usb0", false),
            Step::Set("usb1", false),
            Step::Set("TC_READY_N", true),
        ],
        // Every strap and button released, power untouched.
        Action::Clear => ["pedl", "volup", "voldn", "pkey", "eud"]
            .into_iter()
            .filter(|n| *n != "eud" || has_eud)
            .map(|n| Step::Set(n, false))
            .collect(),
    };
    Ok(steps)
}

/// Boot modes a profile can actually select, for the controller config.
pub fn boot_modes(profile: &TacProfile) -> Vec<String> {
    [
        Action::Edl,
        Action::SailEdl,
        Action::Uefi,
        Action::Fastboot,
        Action::Eud,
        Action::UsbOff,
        Action::UsbOn,
    ]
    .into_iter()
    .filter(|a| sequence(profile, *a, 1500).is_ok())
    .map(|a| a.as_str().to_string())
    .collect()
}

/// Somewhere pins can be driven. Split out so sequences are testable without a
/// board, and so a dry run is the same code path with a different sink.
pub trait PinSink {
    fn set(&mut self, name: &str, value: bool) -> Result<()>;
    fn delay(&mut self, ms: u64);
}

/// Records instead of driving: `--dry-run`, and the test double.
#[derive(Debug, Default)]
pub struct RecordingSink {
    pub steps: Vec<(String, bool)>,
    pub delays_ms: Vec<u64>,
}

impl PinSink for RecordingSink {
    fn set(&mut self, name: &str, value: bool) -> Result<()> {
        self.steps.push((name.to_string(), value));
        Ok(())
    }
    fn delay(&mut self, ms: u64) {
        self.delays_ms.push(ms);
    }
}

/// Run a sequence against any sink.
pub fn run_sequence(sink: &mut impl PinSink, steps: &[Step]) -> Result<()> {
    for step in steps {
        match *step {
            Step::Set(name, v) => sink.set(name, v)?,
            Step::Delay(ms) => sink.delay(ms),
        }
    }
    Ok(())
}

/// A TAC opened for READING ONLY: no interface claimed, no mode set, no write.
///
/// Asking "which board is this?" or "what are the pins doing?" must not disturb
/// anything -- the same rule the Bughopper's `state` follows by taking no claim
/// at all. `READ_PINS` is a device-recipient vendor request, so it needs no
/// interface of its own and cannot take a GPIO channel away from whatever holds
/// it; and unlike an actuating open it never issues `SET_BITMODE`, which
/// reloads the output latch and would drop a level someone is relying on.
pub struct TacProbe {
    pub profile: TacProfile,
    pub serial: String,
    device: nusb::Device,
}

impl TacProbe {
    pub fn open(serial: &str, hint: &str, extra: &[TacProfile]) -> Result<TacProbe> {
        let (info, profile) = Tac::resolve(serial, hint, extra)?;
        let serial = info.serial_number().unwrap_or_default().to_string();
        let device = info
            .open()
            .map_err(|e| usb_err(format!("cannot open TAC {serial}: {e}")))?;
        Ok(TacProbe {
            profile,
            serial,
            device,
        })
    }

    /// Current levels of one GPIO port.
    pub fn read_bus(&self, bus: char) -> Result<u8> {
        use nusb::transfer::{Control, ControlType, Recipient};
        let channel = (bus as u8).saturating_sub(b'A') as u16 + 1;
        let mut buf = [0u8; 1];
        let n = self
            .device
            .control_in_blocking(
                Control {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request: SIO_READ_PINS,
                    value: 0,
                    index: channel,
                },
                &mut buf,
                Duration::from_secs(2),
            )
            .map_err(|e| usb_err(format!("TAC READ_PINS on channel {channel} failed: {e}")))?;
        if n == 0 {
            return Err(usb_err("TAC READ_PINS returned no data".to_string()));
        }
        Ok(buf[0])
    }

    pub fn read_pin(&self, name: &str) -> Result<bool> {
        let p = self
            .profile
            .pin(name)
            .ok_or_else(|| usb_err(format!("no {name:?} pin on this TAC")))?;
        Ok(self.read_bus(p.bus)? & p.mask() != 0)
    }
}

/// An open TAC: the FTDI, its GPIO channels claimed, and a shadow of each port.
pub struct Tac {
    pub profile: TacProfile,
    pub serial: String,
    buses: BTreeMap<char, Bus>,
}

struct Bus {
    iface: nusb::Interface,
    /// FTDI channel number, 1-based: A=1 … D=4. This is the control transfer's
    /// `wIndex`, and it is NOT the USB interface number.
    channel: u16,
    ep_out: u8,
    shadow: u8,
}

impl Tac {
    /// Open the TAC that drives the console at `hint` (a `/dev/serial/by-id`
    /// path), or the one whose serial is `serial`.
    ///
    /// The by-id name embeds both the product string and the FTDI serial —
    /// `usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if00-port0` — so a bench with two TAC
    /// boards resolves each console to its own chip with no configuration, and
    /// an ambiguous match is refused rather than guessed. Guessing here would
    /// power-cycle the wrong board.
    /// Which TAC this selector names, without opening anything.
    ///
    /// Separated from [`open_for`] because identifying a board and taking its
    /// GPIO channels are different acts: `id`, `power-state` and a dry run must
    /// be answerable without claiming an interface or touching a pin.
    pub fn resolve(
        serial: &str,
        hint: &str,
        extra: &[TacProfile],
    ) -> Result<(nusb::DeviceInfo, TacProfile)> {
        let mut candidates: Vec<nusb::DeviceInfo> = nusb::list_devices()
            .map_err(|e| usb_err(format!("cannot enumerate USB: {e}")))?
            .filter(|d| d.vendor_id() == VENDOR_FTDI)
            .collect();
        if !serial.is_empty() {
            candidates.retain(|d| d.serial_number() == Some(serial));
        } else if !hint.is_empty() {
            candidates.retain(|d| d.serial_number().is_some_and(|s| hint.contains(s)));
        }
        // Only FTDIs this build knows how to drive.
        let known: Vec<TacProfile> = extra.iter().cloned().chain(TacProfile::builtin()).collect();
        let mut matched: Vec<(nusb::DeviceInfo, TacProfile)> = candidates
            .into_iter()
            .filter_map(|d| {
                let product = d.product_string()?.to_string();
                let p = known.iter().find(|p| p.usb_descriptor == product)?;
                Some((d, p.clone()))
            })
            .collect();
        match matched.len() {
            0 => Err(usb_err(if serial.is_empty() {
                "no TAC controller found".to_string()
            } else {
                format!("no TAC controller with serial {serial:?}")
            })
            .with_hint(
                "a TAC is recognised by its USB product string; \
                 `lsusb -v -d 0403:` shows it, and an unknown board needs its \
                 .tcnf config added",
            )),
            1 => Ok(matched.remove(0)),
            n => Err(usb_err(format!("{n} TAC controllers match")).with_hint(
                "pass --serial, or --device <by-id path> so the serial can be \
                 resolved: guessing would drive another board's power lines",
            )),
        }
    }

    pub fn open_for(serial: &str, hint: &str, extra: &[TacProfile]) -> Result<Tac> {
        let (info, profile) = Self::resolve(serial, hint, extra)?;
        let serial = info.serial_number().unwrap_or_default().to_string();
        let device = info
            .open()
            .map_err(|e| usb_err(format!("cannot open TAC {serial}: {e}")))?;

        let mut buses = BTreeMap::new();
        for bus in profile.gpio_buses.clone() {
            let ifnum = (bus as u8).saturating_sub(b'A');
            let iface = device
                .detach_and_claim_interface(ifnum)
                .or_else(|_| device.claim_interface(ifnum))
                .map_err(|e| {
                    usb_err(format!(
                        "cannot claim TAC GPIO channel {bus} (interface {ifnum}): {e}"
                    ))
                })?;
            let ep_out = bulk_out_endpoint(&iface)
                .ok_or_else(|| usb_err(format!("TAC channel {bus} has no bulk OUT endpoint")))?;
            let channel = ifnum as u16 + 1;
            let mut b = Bus {
                iface,
                channel,
                ep_out,
                shadow: 0,
            };
            // SEED FROM THE CHIP, do not assume. The vendor tool writes its
            // configured initial values here, which drives `pwr_off` low and
            // powers up a board someone had deliberately switched off. Reading
            // first means an action changes exactly the pins it names.
            b.shadow = b.read_pins().unwrap_or(0);
            b.configure(profile.direction(bus))?;
            buses.insert(bus, b);
        }
        Ok(Tac {
            profile,
            serial,
            buses,
        })
    }

    /// Current levels of one GPIO port, straight off the chip.
    pub fn read_bus(&self, bus: char) -> Result<u8> {
        self.buses
            .get(&bus)
            .ok_or_else(|| usb_err(format!("TAC has no GPIO bus {bus}")))?
            .read_pins()
    }

    /// What this driver last commanded on one port.
    pub fn shadow(&self, bus: char) -> Option<u8> {
        self.buses.get(&bus).map(|b| b.shadow)
    }

    /// Read one named pin off the chip.
    pub fn read_pin(&self, name: &str) -> Result<bool> {
        let p = self
            .profile
            .pin(name)
            .ok_or_else(|| usb_err(format!("no {name:?} pin on this TAC")))?;
        Ok(self.read_bus(p.bus)? & p.mask() != 0)
    }
}

impl PinSink for Tac {
    fn set(&mut self, name: &str, value: bool) -> Result<()> {
        let p = self.profile.pin(name).ok_or_else(|| {
            ToolError::new(
                ErrorCode::HookNotConfigured,
                format!(
                    "no {name:?} pin on this TAC ({})",
                    self.profile.usb_descriptor
                ),
            )
        })?;
        let bus = self
            .buses
            .get_mut(&p.bus)
            .ok_or_else(|| usb_err(format!("TAC bus {} is not open", p.bus)))?;
        let next = if value {
            bus.shadow | p.mask()
        } else {
            bus.shadow & !p.mask()
        };
        bus.write(next)
    }

    fn delay(&mut self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

impl Bus {
    /// Put the channel into async bit-bang with this direction mask, then
    /// restate the levels we read a moment ago.
    ///
    /// The restate is not decoration: `SET_BITMODE` reloads the output latch, so
    /// without it a channel holding `pwr_off` HIGH would drop it and boot the
    /// board the instant anything opened the controller.
    fn configure(&mut self, direction: u8) -> Result<()> {
        self.ctrl(SIO_RESET, 1)?; // purge RX
        self.ctrl(SIO_RESET, 2)?; // purge TX
        self.bitmode(direction)?;
        let held = self.shadow;
        self.write(held)
    }

    fn write(&mut self, value: u8) -> Result<()> {
        let completion = futures::executor::block_on(self.iface.bulk_out(self.ep_out, vec![value]));
        completion
            .into_result()
            .map_err(|e| usb_err(format!("TAC GPIO write {value:#010b} failed: {e}")))?;
        self.shadow = value;
        Ok(())
    }

    fn read_pins(&self) -> Result<u8> {
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
                    index: self.channel,
                },
                &mut buf,
                Duration::from_secs(2),
            )
            .map_err(|e| usb_err(format!("TAC READ_PINS failed: {e}")))?;
        if n == 0 {
            return Err(usb_err("TAC READ_PINS returned no data".to_string()));
        }
        Ok(buf[0])
    }

    fn bitmode(&self, direction: u8) -> Result<()> {
        self.ctrl(
            SIO_SET_BITMODE,
            (BITMODE_ASYNC_BITBANG << 8) | direction as u16,
        )
    }

    fn ctrl(&self, request: u8, value: u16) -> Result<()> {
        use nusb::transfer::{Control, ControlType, Recipient};
        self.iface
            .control_out_blocking(
                Control {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Device,
                    request,
                    value,
                    index: self.channel,
                },
                &[],
                Duration::from_secs(2),
            )
            .map_err(|e| {
                usb_err(format!(
                    "TAC request {request:#04x}({value:#06x}) on channel {} failed: {e}",
                    self.channel
                ))
            })?;
        Ok(())
    }
}

// NO `Drop`. See the module header: these pins are levels, and parking or
// resetting them on the way out would release whatever the action just set --
// turning `power off` into `power off, then straight back on`.

/// The first bulk OUT endpoint of an interface, read from its descriptors
/// rather than assumed. FTDI's layout is regular (`0x02 + 2 * interface`) but
/// reading it costs nothing and cannot be wrong.
fn bulk_out_endpoint(iface: &nusb::Interface) -> Option<u8> {
    iface.descriptors().find_map(|alt| {
        alt.endpoints()
            .find(|e| {
                e.transfer_type() == nusb::transfer::EndpointType::Bulk && e.address() & 0x80 == 0
            })
            .map(|e| e.address())
    })
}

fn usb_err(msg: String) -> ToolError {
    ToolError::new(ErrorCode::HookFailed, msg)
}

fn bad_config(msg: String) -> ToolError {
    ToolError::new(ErrorCode::InvalidArgument, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Step {
        /// `Step::Set` with an owned-looking name, for comparisons in tests.
        #[allow(non_snake_case)]
        fn Set_(name: &str, v: bool) -> Step {
            let name: &'static str = Box::leak(name.to_string().into_boxed_str());
            Step::Set(name, v)
        }
    }

    fn steps_of(action: Action) -> Vec<(String, bool)> {
        let p = TacProfile::ride_micro_4_0();
        let mut sink = RecordingSink::default();
        run_sequence(&mut sink, &sequence(&p, action, 1500).unwrap()).unwrap();
        sink.steps
    }

    /// The pin map is wiring, transcribed from the vendor config. A "tidy" of
    /// any of these numbers drives a different line on a real board.
    #[test]
    fn the_ride_micro_pin_map_matches_its_tac_config() {
        let p = TacProfile::ride_micro_4_0();
        assert_eq!(p.pin("pwr_off"), Some(PinRef { bus: 'D', bit: 1 }));
        assert_eq!(p.pin("sw_dnld"), Some(PinRef { bus: 'D', bit: 5 }));
        assert_eq!(p.pin("ss_force_edl"), Some(PinRef { bus: 'C', bit: 4 }));
        assert_eq!(p.pin("uefi"), Some(PinRef { bus: 'D', bit: 0 }));
        assert_eq!(p.pin("kpd_pwr"), Some(PinRef { bus: 'D', bit: 3 }));
        assert_eq!(p.pin("usb2"), Some(PinRef { bus: 'C', bit: 5 }));
        assert_eq!(p.pin("oe"), Some(PinRef { bus: 'D', bit: 6 }));
        assert_eq!(p.gpio_buses, vec!['C', 'D']);
        // D2 is `enabled: false` on this board: absent, not guessed at.
        assert_eq!(p.pin("pmic_resin"), None);
    }

    #[test]
    fn power_is_a_level_not_a_pulse() {
        assert_eq!(steps_of(Action::Off), [("pwr_off".to_string(), true)]);
        assert_eq!(steps_of(Action::On), [("pwr_off".to_string(), false)]);
    }

    /// EDL must strap BEFORE the rail comes up and release AFTER the sampling
    /// window: strap-then-reset (what a latching controller needs) leaves no QDL
    /// gadget at all. This is the ordering, asserted.
    #[test]
    fn edl_straps_before_power_and_releases_after_the_sampling_window() {
        let p = TacProfile::ride_micro_4_0();
        let steps = sequence(&p, Action::Edl, 1500).unwrap();
        assert_eq!(
            steps,
            vec![
                Step::Set("pwr_off", true),
                Step::Set("sw_dnld", true),
                Step::Set("ss_force_edl", true),
                Step::Delay(1500),
                Step::Set("pwr_off", false),
                Step::Delay(5000),
                Step::Set("sw_dnld", false),
                Step::Set("ss_force_edl", false),
            ]
        );
    }

    #[test]
    fn sail_edl_leaves_the_main_domain_strap_alone() {
        let s = steps_of(Action::SailEdl);
        assert!(s.iter().any(|(n, v)| n == "ss_force_edl" && *v));
        assert!(
            !s.iter().any(|(n, _)| n == "sw_dnld"),
            "SAIL-only EDL must not touch the main domain's strap: {s:?}"
        );
    }

    #[test]
    fn clear_releases_every_strap_and_never_touches_power() {
        let s = steps_of(Action::Clear);
        assert!(!s.is_empty());
        assert!(s.iter().all(|(_, v)| !*v), "clear only releases: {s:?}");
        assert!(
            !s.iter().any(|(n, _)| n == "pwr_off"),
            "clearing a strap must not power the board: {s:?}"
        );
    }

    /// The direction mask is what stops the FTDI and the SoC both driving
    /// RESOUT. Every other pin on that bus stays an output.
    #[test]
    fn the_resout_feedback_line_is_an_input_and_nothing_else_is() {
        let p = TacProfile::ride_micro_4_0();
        assert_eq!(p.direction('C'), 0b1111_1011, "only C2 may be an input");
        assert_eq!(p.direction('D'), 0xFF, "bus D is all outputs");
    }

    /// A sense pin the driver also DRIVES is not a sense pin: it would read back
    /// our own output and report "on" for a board that is off. The two lists
    /// have to agree, on every profile, however they were built.
    #[test]
    fn a_sense_pin_is_always_also_an_input() {
        let mut profiles = TacProfile::builtin();
        profiles.push(
            TacProfile::from_tcnf(
                r#"{"usb_descriptor":"X","bus":[{"bus":"C","bus_function":2}],
                    "pins":[{"bus":"C","command":"md_resout","pin_number":"2","enabled":true},
                            {"bus":"C","command":"pwr_off","pin_number":"1","enabled":true}]}"#,
            )
            .unwrap(),
        );
        for p in profiles {
            let Some(sense) = p.sense.clone() else {
                continue;
            };
            assert!(
                p.inputs.contains(&sense),
                "{}: sense pin {sense:?} is driven as an output, so it reads back \
                 our own command rather than the board",
                p.usb_descriptor
            );
            let pin = p.pin(&sense).expect("sense pin exists");
            assert_eq!(
                p.direction(pin.bus) & pin.mask(),
                0,
                "{}: the direction mask still drives {sense:?}",
                p.usb_descriptor
            );
        }
    }

    /// A board whose config lacks a pin must say so, not drive a line at random.
    #[test]
    fn an_action_a_board_cannot_do_is_refused_by_name() {
        let mut p = TacProfile::ride_micro_4_0();
        p.pins.remove("usb2");
        let e = sequence(&p, Action::Fastboot, 1500).unwrap_err();
        assert_eq!(e.code, ErrorCode::HookNotConfigured);
        assert!(e.message.contains("usb2"), "{}", e.message);
        // …and it drops out of the advertised list rather than lingering.
        assert!(!boot_modes(&p).contains(&"FASTBOOT".to_string()));
        assert!(boot_modes(&p).contains(&"EDL".to_string()));
    }

    /// A vendor `.tcnf` dropped next to the others must load without a rebuild.
    /// This fixture is the real shape, trimmed: two UART buses, two GPIO buses,
    /// one disabled pin and one pin on a UART bus (a second SoC that is not
    /// fitted) — both of which must be ignored.
    #[test]
    fn a_vendor_tac_config_loads_and_drops_what_it_cannot_drive() {
        let json = r#"{
          "usb_descriptor": "RIDE MX 4.0 Lite",
          "bus": [
            {"bus": "A", "bus_function": 1}, {"bus": "B", "bus_function": 1},
            {"bus": "C", "bus_function": 2}, {"bus": "D", "bus_function": 2}
          ],
          "pins": [
            {"bus": "D", "command": "pwr_off",      "pin_number": "1", "enabled": true},
            {"bus": "D", "command": "sw_dnld",      "pin_number": "5", "enabled": true},
            {"bus": "C", "command": "ss_force_edl", "pin_number": "4", "enabled": true},
            {"bus": "C", "command": "md_resout",    "pin_number": "2", "enabled": true},
            {"bus": "D", "command": "pmic_resin",   "pin_number": "2", "enabled": false},
            {"bus": "A", "command": "s2_pwr_off",   "pin_number": "1", "enabled": true}
          ]
        }"#;
        let p = TacProfile::from_tcnf(json).unwrap();
        assert_eq!(p.usb_descriptor, "RIDE MX 4.0 Lite");
        assert_eq!(p.gpio_buses, vec!['C', 'D']);
        assert_eq!(p.pin("pwr_off"), Some(PinRef { bus: 'D', bit: 1 }));
        assert_eq!(p.pin("pmic_resin"), None, "disabled pins are not usable");
        assert_eq!(
            p.pin("s2_pwr_off"),
            None,
            "a pin on a UART channel has no port to drive"
        );
        assert_eq!(p.direction('C'), 0b1111_1011);
        // It can do EDL, and honestly reports that it cannot do UEFI.
        assert_eq!(boot_modes(&p), vec!["EDL", "SAIL_EDL"]);
    }

    /// A freshly booted host shows FOUR ports for this chip, and two of them
    /// are the board's power and strap lines. Opening a tty asserts DTR and RTS,
    /// so a GPIO channel in the console list is a write waiting to happen.
    #[test]
    fn a_tacs_gpio_channels_are_not_consoles() {
        let profiles = TacProfile::builtin();
        for (name, expect) in [
            ("usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if00-port0", None),
            ("usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if01-port0", None),
            ("usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if02-port0", Some('C')),
            ("usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if03-port0", Some('D')),
            // A different FTDI on the same host keeps all of its ports.
            ("usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if02-port0", None),
            (
                "usb-FTDI_NordAU_RIDE_SX_879X_UART_AI41BI4U0R-if02-port0",
                None,
            ),
        ] {
            assert_eq!(gpio_channel_in(name, &profiles), expect, "{name}");
        }
    }

    fn mtp_steps(action: Action) -> Vec<Step> {
        sequence(&TacProfile::alpaca_lite_mtp(), action, 1500).unwrap()
    }

    /// Wiring, transcribed from `TAC_FTDI_13.tcnf`.
    #[test]
    fn the_alpaca_lite_mtp_pin_map_matches_its_tac_config() {
        let p = TacProfile::alpaca_lite_mtp();
        assert_eq!(p.family, TacFamily::Mtp);
        assert_eq!(p.usb_descriptor, "ALPACA-LITE MTP DEBUG BOARD");
        assert_eq!(p.pin("battery"), Some(PinRef { bus: 'D', bit: 1 }));
        assert_eq!(p.pin("pkey"), Some(PinRef { bus: 'D', bit: 3 }));
        assert_eq!(p.pin("pedl"), Some(PinRef { bus: 'D', bit: 5 }));
        assert_eq!(p.pin("TC_READY_N"), Some(PinRef { bus: 'D', bit: 6 }));
        assert_eq!(p.pin("usb0"), Some(PinRef { bus: 'D', bit: 7 }));
        assert_eq!(p.pin("usb1"), Some(PinRef { bus: 'C', bit: 6 }));
        assert_eq!(p.pin("volup"), Some(PinRef { bus: 'D', bit: 0 }));
        assert_eq!(p.pin("voldn"), Some(PinRef { bus: 'D', bit: 2 }));
        assert_eq!(p.gpio_buses, vec!['C', 'D']);
        assert_eq!(p.sense, None, "no feedback line in this config");
        assert_eq!(p.direction('C'), 0xFF);
        assert_eq!(p.direction('D'), 0xFF);
        assert_eq!(p.power_off_pin(), ("battery", true));
    }

    /// Off cuts the battery and both VBUS feeds BEFORE the output enable, so
    /// the first level the board ever sees from us is "off".
    #[test]
    fn mtp_off_cuts_power_before_enabling_the_outputs() {
        let s = mtp_steps(Action::Off);
        let pos = |n: &str, v: bool| s.iter().position(|x| *x == Step::Set_(n, v)).unwrap();
        assert!(pos("battery", true) < pos("TC_READY_N", true), "{s:?}");
        assert!(pos("usb0", true) < pos("TC_READY_N", true), "{s:?}");
        assert!(pos("usb1", true) < pos("TC_READY_N", true), "{s:?}");
        assert!(
            !s.iter().any(|x| *x == Step::Set_("battery", false)),
            "off must never restore the battery: {s:?}"
        );
    }

    #[test]
    fn mtp_on_restores_power_then_presses_the_power_key() {
        let s = mtp_steps(Action::On);
        let pos = |x: Step| s.iter().position(|y| *y == x).unwrap();
        assert!(pos(Step::Set_("battery", false)) < pos(Step::Set_("pkey", true)));
        assert!(pos(Step::Set_("usb0", false)) < pos(Step::Set_("pkey", true)));
        assert_eq!(
            s.last(),
            Some(&Step::Set_("pkey", false)),
            "the key is released"
        );
        assert!(s.contains(&Step::Delay(800)), "the vendor's 800 ms press");
        assert!(
            s.contains(&Step::Delay(2000)),
            "500 ms vendor off-time + settle"
        );
    }

    /// EDL straps before the rail comes back and releases only after the
    /// press: the order the vendor script uses.
    #[test]
    fn mtp_edl_straps_before_power_and_releases_after_the_press() {
        let s = mtp_steps(Action::Edl);
        let pos = |x: Step| s.iter().position(|y| *y == x).unwrap();
        assert!(pos(Step::Set_("pedl", true)) < pos(Step::Set_("battery", false)));
        assert!(pos(Step::Set_("pkey", false)) < pos(Step::Set_("pedl", false)));
        assert!(s.contains(&Step::Delay(1300)), "the config's EDL timing");
        assert_eq!(s.last(), Some(&Step::Set_("pedl", false)));
    }

    /// EUD holds its line from before power-on through the end of the
    /// sequence; any other power-on and `clear` release it.
    #[test]
    fn mtp_eud_holds_its_line_across_power_on_and_nothing_else_does() {
        let s = mtp_steps(Action::Eud);
        let pos = |x: Step| s.iter().position(|y| *y == x).unwrap();
        assert!(pos(Step::Set_("eud", true)) < pos(Step::Set_("battery", false)));
        assert!(!s[pos(Step::Set_("eud", true))..].contains(&Step::Set_("eud", false)));
        assert!(
            s.contains(&Step::Set_("pkey", true)),
            "it must power the board on"
        );
        for a in [
            Action::On,
            Action::Cycle,
            Action::Edl,
            Action::Uefi,
            Action::Fastboot,
        ] {
            let s = mtp_steps(a);
            assert!(
                s.contains(&Step::Set_("eud", false)),
                "{a:?} must release EUD"
            );
            assert!(
                !s.contains(&Step::Set_("eud", true)),
                "{a:?} must not assert EUD"
            );
        }
        assert!(mtp_steps(Action::Clear).contains(&Step::Set_("eud", false)));
        let ride = TacProfile::ride_micro_4_0();
        assert_eq!(
            sequence(&ride, Action::Eud, 1500).unwrap_err().code,
            ErrorCode::HookNotConfigured
        );
        assert!(!boot_modes(&ride).contains(&"EUD".to_string()));
    }

    /// USB_OFF/USB_ON move only the two VBUS feeds: the battery line, which
    /// keeps the board running, is never touched, and nothing is pressed.
    #[test]
    fn mtp_usb_off_and_on_touch_only_the_vbus_feeds() {
        for (a, level) in [(Action::UsbOff, true), (Action::UsbOn, false)] {
            let s = mtp_steps(a);
            assert!(s.contains(&Step::Set_("usb0", level)), "{a:?}: {s:?}");
            assert!(s.contains(&Step::Set_("usb1", level)), "{a:?}: {s:?}");
            assert!(
                !s.iter().any(|x| matches!(
                    x,
                    Step::Set("battery" | "pkey" | "pedl" | "volup" | "voldn" | "eud", _)
                        | Step::Delay(_)
                )),
                "{a:?} must not touch power, buttons or straps: {s:?}"
            );
        }
        let ride = TacProfile::ride_micro_4_0();
        for a in [Action::UsbOff, Action::UsbOn] {
            assert_eq!(
                sequence(&ride, a, 1500).unwrap_err().code,
                ErrorCode::HookNotConfigured
            );
        }
    }

    /// A fake FTDI: every level change stamped with a virtual clock that only
    /// the sequence's own delays advance.
    #[derive(Default)]
    struct FakeFtdi {
        now_ms: u64,
        events: Vec<(u64, String, bool)>,
    }

    impl PinSink for FakeFtdi {
        fn set(&mut self, name: &str, value: bool) -> Result<()> {
            self.events.push((self.now_ms, name.to_string(), value));
            Ok(())
        }
        fn delay(&mut self, ms: u64) {
            self.now_ms += ms;
        }
    }

    impl FakeFtdi {
        fn run(action: Action, hold_ms: u64) -> FakeFtdi {
            let mut f = FakeFtdi::default();
            let steps =
                sequence_timed(&TacProfile::alpaca_lite_mtp(), action, 1500, hold_ms).unwrap();
            run_sequence(&mut f, &steps).unwrap();
            f
        }

        /// The times `pin` was driven to `level`.
        fn at(&self, pin: &str, level: bool) -> Vec<u64> {
            self.events
                .iter()
                .filter(|(_, n, v)| n == pin && *v == level)
                .map(|(t, _, _)| *t)
                .collect()
        }
    }

    /// FASTBOOT holds volume down from before power-on until `hold_ms` after
    /// the power-key release, then lets go once: U-Boot reads the key about 5
    /// to 6 s after the release, long after the vendor's 1.8 s.
    #[test]
    fn mtp_fastboot_holds_volume_down_for_the_configured_time_after_the_key() {
        for hold in [crate::tac::MTP_FASTBOOT_HOLD_MS, 4_000] {
            let f = FakeFtdi::run(Action::Fastboot, hold);
            let press = f.at("pkey", true);
            let release = f.at("pkey", false);
            let down = f.at("voldn", true);
            let up = f.at("voldn", false);
            assert_eq!(press.len(), 1, "one power-key press: {:?}", f.events);
            assert_eq!(down.len(), 1, "volume down pressed once: {:?}", f.events);
            assert_eq!(
                up.len(),
                1,
                "and released once, no re-press: {:?}",
                f.events
            );
            let key_up = *release.last().unwrap();
            assert_eq!(key_up - press[0], 800, "the vendor's 800 ms press");
            assert!(
                down[0] <= f.at("battery", false)[0],
                "held before the battery is restored"
            );
            assert_eq!(up[0] - key_up, hold, "released {hold} ms after the key");
            assert_eq!(
                f.events.last().map(|(_, n, v)| (n.as_str(), *v)),
                Some(("voldn", false)),
                "the release is the last thing the sequence does"
            );
            // 500 ms vendor off-time + 1500 settle + 900 + 30, then the press.
            assert_eq!(press[0], 2_930);
            assert_eq!(f.now_ms, 2_930 + 800 + hold);
        }
    }

    /// The hold is the MTP's: the RIDE selects fastboot with its `usb2` strap
    /// and its sequence does not change with it.
    #[test]
    fn the_fastboot_hold_does_not_touch_the_ride_sequence() {
        let ride = TacProfile::ride_micro_4_0();
        assert_eq!(
            sequence_timed(&ride, Action::Fastboot, 1500, 1).unwrap(),
            sequence_timed(&ride, Action::Fastboot, 1500, 50_000).unwrap()
        );
    }

    #[test]
    fn mtp_modes_and_refusals() {
        let p = TacProfile::alpaca_lite_mtp();
        assert_eq!(
            boot_modes(&p),
            vec!["EDL", "UEFI", "FASTBOOT", "EUD", "USB_OFF", "USB_ON"]
        );
        let e = sequence(&p, Action::SailEdl, 1500).unwrap_err();
        assert_eq!(e.code, ErrorCode::HookNotConfigured);
        let clear = mtp_steps(Action::Clear);
        assert!(
            clear.iter().all(|x| matches!(x, Step::Set(_, false))),
            "{clear:?}"
        );
        assert!(
            !clear
                .iter()
                .any(|x| matches!(x, Step::Set("battery" | "usb0" | "usb1", _))),
            "clearing must not touch power: {clear:?}"
        );
        // Every sequence leaves every button and strap released.
        for a in [
            Action::On,
            Action::Cycle,
            Action::Edl,
            Action::Uefi,
            Action::Fastboot,
        ] {
            let mut sink = RecordingSink::default();
            run_sequence(&mut sink, &mtp_steps(a)).unwrap();
            for n in ["pkey", "volup", "voldn", "pedl"] {
                let last = sink
                    .steps
                    .iter()
                    .rev()
                    .find(|(m, _)| m == n)
                    .map(|(_, v)| *v);
                assert_eq!(last, Some(false), "{a:?} leaves {n} held");
            }
        }
    }

    #[test]
    fn the_mtp_tacs_gpio_channels_are_not_consoles() {
        let profiles = TacProfile::builtin();
        let n = "usb-FTDI_ALPACA-LITE_MTP_DEBUG_BOARD_FT0MTP01-if0";
        assert_eq!(gpio_channel_in(&format!("{n}0-port0"), &profiles), None);
        assert_eq!(gpio_channel_in(&format!("{n}1-port0"), &profiles), None);
        assert_eq!(
            gpio_channel_in(&format!("{n}2-port0"), &profiles),
            Some('C')
        );
        assert_eq!(
            gpio_channel_in(&format!("{n}3-port0"), &profiles),
            Some('D')
        );
    }

    /// A dropped-in config with the MTP pin names gets the MTP sequences.
    #[test]
    fn a_vendor_config_with_mtp_pins_is_an_mtp_board() {
        let json = r#"{
          "usb_descriptor": "ALPACA-LITE OTHER MTP",
          "bus": [{"bus": "C", "bus_function": 2}, {"bus": "D", "bus_function": 2}],
          "pins": [
            {"bus": "C", "command": "usb1",       "pin_number": "6", "enabled": true},
            {"bus": "D", "command": "volup",      "pin_number": "0", "enabled": true},
            {"bus": "D", "command": "battery",    "pin_number": "1", "enabled": true},
            {"bus": "D", "command": "voldn",      "pin_number": "2", "enabled": true},
            {"bus": "D", "command": "pkey",       "pin_number": "3", "enabled": true},
            {"bus": "D", "command": "pedl",       "pin_number": "5", "enabled": true},
            {"bus": "D", "command": "TC_READY_N", "pin_number": "6", "enabled": true},
            {"bus": "D", "command": "usb0",       "pin_number": "7", "enabled": true}
          ]
        }"#;
        let p = TacProfile::from_tcnf(json).unwrap();
        assert_eq!(p.family, TacFamily::Mtp);
        assert_eq!(
            boot_modes(&p),
            vec!["EDL", "UEFI", "FASTBOOT", "USB_OFF", "USB_ON"],
            "no eud pin, so no EUD; the VBUS feeds are there"
        );
    }

    #[test]
    fn a_config_with_nothing_drivable_is_an_error_not_an_empty_controller() {
        let json = r#"{"usb_descriptor":"X","bus":[{"bus":"A","bus_function":1}],
                       "pins":[{"bus":"A","command":"p","pin_number":"0","enabled":true}]}"#;
        assert!(TacProfile::from_tcnf(json).is_err());
    }
}
