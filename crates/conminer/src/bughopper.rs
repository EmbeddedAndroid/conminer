//! Power / reset / EDL for a Bughopper-class FTDI CBUS controller.
//!
//! The controller profile invokes this as a hook. It is a conminer subcommand
//! rather than a script for one reason that matters on the bench: claiming the
//! FTDI detaches `ftdi_sio`, so the board's console node disappears while the
//! action runs. Doing the work in-process means the same process that took the
//! interface gives it back (see `Cbus`'s `Drop`), instead of a separate cleanup
//! pass that might not run — a timed-out hook is SIGKILLed and never gets to
//! tidy up, which is exactly how a console goes missing for hours.

use anyhow::{bail, Result};
use conminer_core::ftdi::{mask, Cbus};
use std::time::Duration;

const PRE_RESET: Duration = Duration::from_millis(100);
const EDL_HOLD_S: f64 = 8.0;

pub fn run(
    action: &str,
    arg: &str,
    settle: f64,
    serial: &str,
    device: &str,
    no_rebind: bool,
) -> Result<()> {
    // A STATE QUERY MUST NEVER CLAIM THE INTERFACE. Claiming detaches
    // `ftdi_sio`, and this controller's console IS that interface. The guard
    // below existed for `state`, but the profile asks `power-state`, which
    // fell through to `Cbus::open_for` just to print "unknown" -- so every
    // diagnose, every `list_devices {power: true}`, every sense check inside a
    // verification bounced the Uno Q's console off the bus (measured on bravo:
    // ftdi_sio detach/attach in the same second as each `diagnose` call, and a
    // ser2net "dev read error" for every one). There is no sense line to read
    // anyway: the answer is "unknown" whether or not the pins are read, so it
    // is answered here without touching USB.
    if state_query(action) {
        println!("unknown (commanded-only controller; no sense line back from the board)");
        println!("bughopper-power: {action} ok");
        return Ok(());
    }

    let cbus = Cbus::open_for(serial, device)?;
    let hold = Duration::from_secs_f64(settle.clamp(0.1, 60.0));

    // Run the action, then ALWAYS hand the interface back -- including when the
    // action failed. A power command that errors halfway must not also take the
    // board's console with it, which is exactly what an early `?` would do.
    let outcome = (|| -> Result<()> {
        match action {
            "id" => println!("  bughopper FTDI CBUS controller"),
            "on" => {
                cbus.hold(mask::RESET_ASSERT, PRE_RESET, mask::ALL_LOW)?;
                println!("  power on (PM_RESIN_N short press)");
            }
            "off" => {
                // CBUS1, not CBUS2. Measured on hardware: a long hold of CBUS2
                // (HIL's MASK_RESET_ASSERT, "PM_RESIN_N") RESTARTS this board --
                // every `off` produced a fresh capture epoch with idle_ms in the
                // tens of ms. A hold of CBUS1 (HIL's MASK_MPU_RESET) leaves it
                // down: idle_ms 25157 and climbing. The upstream names describe
                // a different board's wiring; these follow what the hardware
                // actually does.
                cbus.hold(mask::MPU_RESET, hold, mask::MPU_DONE)?;
                println!("  power off (held {settle}s)");
            }
            "cycle" => {
                // Off on CBUS1, settle, then on via the CBUS2 short press.
                cbus.hold(mask::MPU_RESET, hold, mask::MPU_DONE)?;
                std::thread::sleep(Duration::from_secs(1));
                cbus.hold(mask::RESET_ASSERT, PRE_RESET, mask::ALL_LOW)?;
                println!("  power cycled (off {settle}s, then on)");
            }
            "reset" => {
                // CBUS2 pulsed briefly is this board's reset.
                cbus.hold(mask::RESET_ASSERT, PRE_RESET, mask::ALL_LOW)?;
                println!("  reset");
            }
            "mode"
                if matches!(
                    arg.to_ascii_lowercase().as_str(),
                    "clear" | "none" | "normal"
                ) =>
            {
                // This controller sequences EDL and releases the strap itself, so
                // there is nothing latched to clear. Succeeding (rather than
                // erroring) keeps "clear the straps" a safe, uniform step an
                // operator can run on ANY board without knowing its controller.
                println!("  no latched straps on this controller; nothing to clear");
            }
            "mode" => {
                let mode = arg.to_ascii_uppercase();
                if !matches!(mode.as_str(), "EDL" | "BOOT_MD_EDL") {
                    bail!(
                        "unknown mode {mode:?}: CBUS carries only FORCED_USB_BOOT_N, so EDL only"
                    );
                }
                // Order matters, and an earlier version had it wrong in a way that
                // powered the board DOWN instead of strapping it: it held
                // PM_RESIN_N for the full 8s, which is a PMIC long-press power-off,
                // not a reset pulse.
                //
                // The correct sequence (from the HIL's enter_edl): pulse reset
                // BRIEFLY, then release reset while FORCED_USB_BOOT_N is asserted,
                // so the SoC samples the strap as it comes out of reset. The hold
                // spans the sampling window, not the reset itself.
                cbus.set(mask::RESET_ASSERT)?;
                std::thread::sleep(PRE_RESET);
                cbus.set(mask::EDL_HOLD)?;
                std::thread::sleep(Duration::from_secs_f64(EDL_HOLD_S));
                cbus.set(mask::ALL_LOW)?;
                println!("  entered EDL (reset pulsed, FORCED_USB_BOOT_N held {EDL_HOLD_S}s)");
            }
            other => bail!("unknown action {other:?}"),
        }

        Ok(())
    })();

    // Park the lines and leave bit-bang, then give the interface back to
    // ftdi_sio so the console node returns. The kernel does NOT do this on close
    // -- measured on hardware -- so it is explicit.
    drop(cbus);
    // Rebinding resets the FTDI, which returns CBUS to its EEPROM defaults and
    // can pulse the lines we just drove. Measured: `off` powered the board down
    // and the rebind brought it straight back up -- a new capture epoch started
    // and the console kept talking, which read as "off does not work".
    let n = if no_rebind {
        0
    } else {
        conminer_core::ftdi::rebind_ftdi_sio()
    };
    outcome?;
    if n > 0 {
        println!("  restored {n} console interface(s) to ftdi_sio");
    }
    println!("bughopper-power: {action} ok");
    Ok(())
}

/// The actions that only ASK. None of them may open the FTDI: `Cbus::open_for`
/// claims the interface, and claiming detaches the console.
///
/// This controller has NO sense line back from the board: its CBUS pins are
/// outputs driving the power button, and the parked state after any action is
/// ALL_LOW, so even a pin readback could not tell "off" from "idle after
/// powering on". The honest answer is "unknown" -- the Bantam boards taught
/// that PWR_OFF (the request) and MD_PS_HOLD (the truth) disagree exactly when
/// something has gone wrong -- and it costs nothing to give.
fn state_query(action: &str) -> bool {
    matches!(action, "state" | "power-state")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A power-state probe on a device that does not exist must still answer:
    /// if it reached `Cbus::open_for` it would fail (or, on a live bench, claim
    /// the interface and drop the console).
    #[test]
    fn a_power_state_query_never_opens_the_ftdi() {
        for action in ["state", "power-state"] {
            assert!(state_query(action), "{action} is a query");
            run(
                action,
                "",
                6.0,
                "",
                "/dev/serial/by-id/usb-No_Such_Bughopper-if00-port0",
                true,
            )
            .unwrap_or_else(|e| panic!("{action} must answer without the device: {e}"));
        }
    }

    #[test]
    fn every_actuation_still_takes_the_interface() {
        for action in ["on", "off", "cycle", "reset", "mode", "id"] {
            assert!(
                !state_query(action),
                "{action} actuates (or identifies) and needs the FTDI"
            );
        }
    }
}
