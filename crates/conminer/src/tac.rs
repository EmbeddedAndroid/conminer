//! Power / reset / EDL / UEFI / fastboot for an FTDI TAC ("Alpaca") board.
//!
//! The hook end of `conminer_core::tac`. Native rather than a script for the
//! same reason the Bughopper's is, plus one specific to this controller: its
//! GPIO channels are LEVELS, so an action that is killed halfway can leave a
//! strap latched. Doing it in-process keeps the sequence and its release in one
//! place, and `boot_mode clear` exists as the escape hatch when something did
//! get killed.

use anyhow::{bail, Result};
use conminer_core::tac::{
    boot_modes, catalogue, run_sequence, sequence, Action, PinSink, Step, Tac, TacProbe, TacProfile,
};

pub fn run(
    action: &str,
    arg: &str,
    settle: f64,
    serial: &str,
    device: &str,
    dry_run: bool,
) -> Result<()> {
    // The SAME catalogue discovery uses to keep GPIO channels out of the console
    // list. Two loaders would eventually disagree about which channels are
    // consoles, and the disagreement would show up as a port that works until
    // someone powers the board.
    let extra = catalogue();
    let settle_ms = (settle.clamp(0.1, 60.0) * 1000.0) as u64;

    // QUESTIONS FIRST, and before the action vocabulary is consulted at all.
    // `id` and `power-state` are not board actions and have no pin sequence, so
    // parsing them as one rejected both with "unknown TAC action" -- measured on
    // the bravo host, where the read-only probe was unreachable through the very
    // path built for it.
    //
    // They go through the read-only probe: no interface taken from whatever
    // holds it, and no SET_BITMODE, which reloads the FTDI's output latch and
    // would drop the very level a `power off` is holding.
    if action == "id" {
        let probe = TacProbe::open(serial, device, extra)?;
        println!(
            "  TAC {} serial {} buses {:?}",
            probe.profile.usb_descriptor, probe.serial, probe.profile.gpio_buses
        );
        for bus in &probe.profile.gpio_buses {
            match probe.read_bus(*bus) {
                Ok(v) => println!("    bus {bus} = {v:#010b}"),
                Err(e) => println!("    bus {bus} = unreadable ({})", e.message),
            }
        }
        println!("tac-power: id ok");
        return Ok(());
    }

    if action == "power-state" {
        let tac = TacProbe::open(serial, device, extra)?;
        // THE MEASUREMENT WINS, and the request is shown beside it.
        //
        // `pwr_off` is only what was last COMMANDED. The sense pin is the SoC
        // driving a line back through the TAC's buffer, so it is evidence:
        // measured across five power cycles on the IQ8, 1 while running and 0
        // while off. When the two disagree -- commanded on, sensed off -- that
        // is a board that did not come up, which is exactly the case a
        // commanded-only controller reports as "on" and sends somebody hunting
        // a console bug. So the sensed value is the answer, and the commanded
        // one rides along as context.
        //
        // The first token is the contract: conminer parses it by equality.
        let commanded = tac.read_pin("pwr_off").ok().map(|off| !off);
        let sensed = tac
            .profile
            .sense
            .as_deref()
            .and_then(|pin| tac.read_pin(pin).ok());
        let note = |on: bool| if on { "on" } else { "off" };
        match (sensed, commanded) {
            (Some(s), Some(c)) => println!(
                "{} (sensed {}={}, commanded={})",
                note(s),
                tac.profile.sense.as_deref().unwrap_or("sense"),
                u8::from(s),
                note(c)
            ),
            (Some(s), None) => println!("{}", note(s)),
            // No sense line on this board type: say unknown rather than dress
            // the request up as a measurement.
            (None, Some(c)) => println!("unknown (commanded={}, no sense line)", note(c)),
            (None, None) => println!("unknown"),
        }
        println!("tac-power: power-state ok");
        return Ok(());
    }

    let act = match action {
        "mode" => match Action::parse(arg) {
            Some(a) => a,
            None => bail!(
                "unknown TAC boot mode {arg:?}; this controller offers {:?} (or \"clear\")",
                known_modes(extra)
            ),
        },
        other => match Action::parse(other) {
            Some(a) => a,
            None => bail!("unknown TAC action {other:?}"),
        },
    };

    // A dry run resolves the board and prints the exact pin sequence, claiming
    // nothing: the same answer `power {dry_run: true}` gives for a script hook,
    // for a controller that has no argv to show. It resolves through the same
    // USB match, so it still proves WHICH board would have been driven.
    if dry_run {
        let (_, profile) = Tac::resolve(serial, device, extra)?;
        let steps = sequence(&profile, act, settle_ms)?;
        println!("  TAC {} ({})", profile.usb_descriptor, act.as_str());
        for step in &steps {
            match step {
                Step::Set(n, v) => println!("    {n} = {}", u8::from(*v)),
                Step::Delay(ms) => println!("    delay {ms}ms"),
            }
        }
        println!("tac-power: dry run, nothing driven");
        return Ok(());
    }

    // Everything past here DRIVES the board, so this is where the interface is
    // claimed.
    let mut tac = Tac::open_for(serial, device, extra)?;
    let steps = sequence(&tac.profile, act, settle_ms)?;
    // The TAC's output enable gates the software-download and RESOUT paths, and
    // its configured initial value is HIGH. Assert it before any sequence that
    // uses a strap, and never as part of the sequence itself -- it is a
    // precondition, not a step.
    if tac.profile.pin("oe").is_some() && !matches!(act, Action::On | Action::Off) {
        tac.set("oe", true)?;
    }
    run_sequence(&mut tac, &steps)?;

    println!("  TAC {} {}", tac.profile.usb_descriptor, act.as_str());
    for step in &steps {
        if let Step::Set(n, v) = step {
            println!("    {n} = {}", u8::from(*v));
        }
    }
    println!("tac-power: {} ok", act.as_str());
    Ok(())
}

fn known_modes(extra: &[TacProfile]) -> Vec<String> {
    extra
        .first()
        .cloned()
        .or_else(|| TacProfile::builtin().into_iter().next())
        .map(|p| boot_modes(&p))
        .unwrap_or_default()
}
