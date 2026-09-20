//! Suite `deploy` — the contracts between conminer and the things it ships with.
//!
//! Every defect that broke the live deployment lived here rather than in any
//! module: the generated ser2net config was a format the packaged ser2net
//! cannot read, services resolved each other's addresses as if they shared a
//! network namespace, and the multi-consumer premise depended on a package
//! feature nobody checked for. None were reachable by a unit test, and all of
//! them were one grep away from being caught.
//!
//! These tests read the Dockerfile and compose file as the source of truth for
//! what actually gets deployed.

use conminer_core::config::Config;
use conminer_core::discovery::ser2net_config;
use conminer_core::store::{DeviceRow, IdentityKind, Registry};

fn dockerfile() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Dockerfile"))
        .expect("Dockerfile is part of the deployment contract")
}

fn compose() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docker-compose.yaml"
    ))
    .expect("docker-compose.yaml is part of the deployment contract")
}

fn one_device_config() -> String {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = Registry::open(dir.path()).unwrap();
    let cfg = Config::default();
    let d: DeviceRow = reg
        .upsert_device(
            "/dev/serial/by-id/usb-FTDI_X-if00-port0",
            None,
            IdentityKind::ById,
            None,
            1,
        )
        .unwrap();
    reg.assign_port(d.id, cfg.ser2net.base_port).unwrap();
    ser2net_config(&reg.all_devices().unwrap(), &cfg)
}

#[test]
fn the_generated_config_matches_the_ser2net_the_image_installs() {
    // The defect this exists for: the image shipped ser2net 3.5.1 while
    // conminer generated 4.x YAML. 3.x cannot read it at all — it starts, binds
    // nothing, logs nothing, and looks exactly like a lab where every console
    // has gone quiet. Live capture was dead from the first deploy.
    let df = dockerfile();
    let text = one_device_config();
    let generates_v4 = text.contains("%YAML") && text.contains("connection: &");

    // Where 4.x actually comes from. Alpine stable ships 3.x and only edge has
    // 4.x, which is why this image is Debian: trixie packages ser2net 4.6.4, and
    // one libc across builder and runtime avoids the musl-static-on-glibc exec
    // failure that killed the first attempt.
    let pins_v4 = df.contains("ser2net")
        && (df.contains("debian:trixie")
            || df.contains("rust:1-trixie")
            || df.contains("alpine/edge"));
    assert_eq!(
        generates_v4,
        pins_v4,
        "conminer generates {} config but the Dockerfile installs {}. \
         These must agree or ser2net silently serves nothing.",
        if generates_v4 {
            "ser2net 4.x YAML"
        } else {
            "ser2net 3.x"
        },
        if pins_v4 {
            "4.x from edge"
        } else {
            "the stable 3.x package"
        },
    );
}

#[test]
fn the_multi_consumer_premise_needs_a_ser2net_that_supports_it() {
    // conminer's whole composition story is that minerd captures while the
    // dashboard watches and minicom attaches too. ser2net 3.x serves exactly
    // one client per port and answers the second with "Port already in use",
    // so emitting `max-connections` against 3.x promises something the package
    // cannot deliver.
    let text = one_device_config();
    if text.contains("max-connections") {
        let df = dockerfile();
        assert!(
            df.contains("debian:trixie")
                || df.contains("rust:1-trixie")
                || df.contains("alpine/edge"),
            "max-connections is a ser2net 4.x feature; the image must install 4.x \
             or the dashboard can never attach alongside minerd"
        );
    }
}

#[test]
fn services_do_not_reach_each_other_over_loopback() {
    // Three separate outages came from this one assumption: mcpd bound
    // 127.0.0.1 inside its container so its published port reset every
    // connection, and both minerd and dashd derived ser2net's address from its
    // *bind* address, landing on 127.0.0.1 — themselves. Under compose each
    // service is its own network namespace, so loopback is never the way to
    // another service.
    let c = compose();
    // Enumerated, not listed by hand: this bug was fixed three times, in
    // minerd, then dashd, then mcpd, because each fix only covered the service
    // that happened to fail. Every service that opens a console endpoint needs
    // the sibling-container address.
    for (service, var, expected) in [
        ("minerd", "CONMINER_SER2NET_HOST", "ser2net"),
        ("dashd", "CONMINER_SER2NET_HOST", "ser2net"),
        ("mcpd", "CONMINER_SER2NET_HOST", "ser2net"),
        ("mcpd", "CONMINER_MCPD_BIND", "0.0.0.0"),
    ] {
        let block = c
            .split_once(&format!("container_name: conminer-{service}"))
            .unwrap_or_else(|| panic!("no {service} service in compose"))
            .1;
        // Only look within this service's own block.
        let block = block.split("container_name:").next().unwrap_or(block);
        assert!(
            block.contains(&format!("{var}: {expected}")),
            "{service} must set {var}={expected}; without it, it resolves a sibling \
             container to its own loopback and silently never connects"
        );
    }
}

/// TWO RULES, AND THEY POINT OPPOSITE WAYS ON PURPOSE.
///
/// Deployed, the dashboard must reach mcpd by SERVICE NAME: a loopback default
/// makes every power button fail with a connection error the moment it runs in
/// compose. Under test it must reach NOTHING: the dev container shares the
/// bench's docker network, so that same service name resolves there to the mcpd
/// driving real boards, and a rig posting `/api/power/...` was one resolvable
/// selector away from moving hardware.
///
/// Asserted on the branches themselves rather than on `Config::default()`, which
/// answers whichever way the environment running this suite happens to point --
/// and inside the container it points at the test one, so this test used to read
/// the wrong rule and fail.
#[test]
fn the_dashboard_reaches_mcpd_by_service_name_not_loopback() {
    let deployed = conminer_core::config::default_mcp_url(false);
    assert!(
        !deployed.contains("127.0.0.1") && !deployed.contains("localhost"),
        "dashboard.mcp_url is {deployed:?}, which cannot reach mcpd from another container"
    );
    assert!(deployed.contains("mcpd"), "{deployed}");

    let under_test = conminer_core::config::default_mcp_url(true);
    assert!(
        !under_test.contains("mcpd"),
        "a test run must not be pointed at the live service: {under_test}"
    );
}

#[test]
fn the_hook_script_is_mounted_where_the_hooks_actually_run() {
    // Hooks run in mcpd. Mounting the script anywhere else produces
    // HOOK_FAILED "No such file or directory" at the moment someone presses a
    // power button.
    let c = compose();
    let mcpd = c
        .split_once("container_name: conminer-mcpd")
        .expect("mcpd service")
        .1;
    let mcpd = mcpd.split("container_name:").next().unwrap_or(mcpd);
    assert!(
        mcpd.contains("bantam-power"),
        "the power hook must be mounted into mcpd, which is where hooks execute"
    );
    assert!(
        mcpd.contains("166:*"),
        "mcpd needs a device cgroup rule for ttyACM* to reach the controller"
    );
}

#[test]
fn every_compose_service_runs_a_real_conminer_subcommand() {
    // A typo in a command lands as a container that restarts forever.
    let c = compose();
    let known = [
        "discoveryd",
        "minerd",
        "mcpd",
        "dashd",
        "peerd",
        "ser2net-supervisor",
    ];
    for line in c.lines().filter(|l| l.trim_start().starts_with("command:")) {
        let cmd = line
            .split('"')
            .nth(1)
            .unwrap_or_else(|| panic!("unparseable command line: {line}"));
        assert!(
            known.contains(&cmd),
            "unknown subcommand {cmd:?} in compose"
        );
    }
}

/// The supervisor must verify ser2net is LISTENing after a reload, not assume
/// it. The defect this exists for: a SIGHUP reload tore down every accepter
/// while existing sessions survived, so minerd kept streaming, the healthcheck
/// stayed green, and every new console attach was refused with "connection
/// refused" for hours.
#[test]
fn the_supervisor_verifies_listeners_after_a_reload() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/service.rs"))
        .expect("service.rs");

    assert!(
        src.contains("fn listening_ports"),
        "no way to observe listeners"
    );
    assert!(
        src.contains("fn configured_ports"),
        "no way to know what should listen"
    );
    // 0A is LISTEN; checking for ESTABLISHED instead is exactly the mistake that
    // made the original failure invisible.
    assert!(
        src.contains("\"0A\""),
        "must distinguish LISTEN from ESTABLISHED"
    );
    assert!(
        src.contains("no listener on configured ports"),
        "a missing accepter must restart ser2net, not report healthy"
    );
    // Not only after a reload: ser2net was measured coming up with three of four
    // ports bound, nothing logged and the healthcheck green, so the check has to
    // run continuously.
    assert!(
        src.contains("ticks % 60 == 0"),
        "listener verification must be periodic, not only post-reload"
    );
    // ...but never so eager that it kills a ser2net that is still binding.
    assert!(
        src.contains("COOLDOWN_TICKS"),
        "a restart must be followed by a cooldown or the check becomes a restart loop"
    );
    // And the rewrite storm that triggered it must be debounced.
    assert!(
        src.contains("Debounce"),
        "config churn must not SIGHUP on every rewrite"
    );
}

/// fd churn: a device that disappears and comes straight back regenerates a
/// byte-identical config. Reacting to the rewrite rather than to a real change
/// is what leaves ser2net with no accepters, so the supervisor must compare
/// content, restart gracefully, and surface ser2net's own diagnostics.
#[test]
fn the_supervisor_survives_file_descriptor_churn() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/service.rs"))
        .expect("service.rs");

    // Content, not mtime: an identical rewrite must not trigger anything.
    assert!(
        src.contains("fn config_digest"),
        "config changes must be content-based"
    );
    assert!(
        !src.contains("m.modified()"),
        "mtime comparison reacts to no-op rewrites"
    );

    // Respawning without waiting races the old process for its own accepters.
    assert!(
        src.contains("fn stop_ser2net"),
        "no graceful stop before respawn"
    );
    assert!(
        src.contains("Address already in use") || src.contains("TIME_WAIT"),
        "the reason for the settle must be recorded where it can be read"
    );

    // ser2net's per-port failures only exist on its own stderr.
    assert!(src.contains("\"-d\""), "ser2net must be asked to log");
    assert!(
        src.contains("ser2net: {line}"),
        "ser2net output must reach our log"
    );
}

/// Claiming an FTDI detaches ftdi_sio and the console node goes with it. The
/// kernel does NOT rebind on fd close -- measured on hardware, where the
/// console stayed missing across every subsequent power action. pyftdi does not
/// rely on that either (pyusb calls libusb_attach_kernel_driver explicitly),
/// which is why the HIL never saw this.
#[test]
fn a_detached_console_is_always_handed_back_to_ftdi_sio() {
    let ftdi = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/ftdi.rs"
    ))
    .expect("ftdi.rs");
    let hook = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/bughopper.rs"))
        .expect("bughopper.rs");
    let svc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/service.rs"))
        .expect("service.rs");

    // An explicit rebind exists, and binds only what has no driver.
    assert!(ftdi.contains("fn rebind_ftdi_sio"), "no explicit rebind");
    // Reattaching an already-bound interface is a no-op at the kernel, so the
    // "only if unbound" guard is unnecessary here -- but a genuinely absent
    // device must still not be conjured back, which enumeration handles.
    assert!(
        ftdi.contains("nusb::list_devices"),
        "only reattach devices that exist"
    );
    // usbfs ioctl, not a sysfs write: sysfs is read-only in a container, which
    // is why every sysfs rebind attempt logged "could not rebind ftdi_sio".
    // This is the same call libusb's attach_kernel_driver makes, which is why
    // pyftdi-based tooling never loses a console.
    assert!(
        ftdi.contains("USBDEVFS_CONNECT"),
        "must reattach via usbfs ioctl"
    );
    // The WRITE, not the word: the comment above legitimately explains why the
    // sysfs route was abandoned.
    assert!(
        !ftdi.contains(r#"fs::write("/sys/bus/usb/drivers"#),
        "a sysfs driver bind cannot work from a container"
    );
    // The sysfs path, not the word: the prose above legitimately explains why
    // resets are avoided.
    assert!(
        !ftdi.contains("/authorized"),
        "must not poke USB authorization; bind the driver, do not reset the port"
    );

    // The hook rebinds on EVERY path, including when the action failed.
    assert!(
        hook.contains("rebind_ftdi_sio()"),
        "hook does not restore the console"
    );
    assert!(
        hook.contains("outcome?;"),
        "the rebind must run before the error propagates, or a failed power \
         command also takes the console with it"
    );

    // And discoveryd is the safety net for tools conminer does not own.
    assert!(
        svc.contains("rebind_ftdi_sio"),
        "no recovery for foreign detaches"
    );
}

/// A TAC's pins are LEVELS, and three rules follow from that. Each one, if
/// "tidied" away, turns `power off` into `power off then straight back on` --
/// a failure that only shows up on hardware, at the worst moment, so it is
/// pinned here.
#[test]
fn a_tac_holds_its_levels_and_answers_questions_without_taking_anything() {
    let core = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/tac.rs"
    ))
    .expect("core tac.rs");
    let hook = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tac.rs"))
        .expect("tac.rs");

    // 1. No Drop. `Cbus` parks its lines and leaves bit-bang, which is right for
    //    a momentary button and catastrophic for a level: leaving bit-bang
    //    releases `pwr_off` and powers the board back up on the way out.
    assert!(
        !core.contains("impl Drop for Tac"),
        "a Drop on the TAC would release the level the action just set"
    );

    // 2. Opening seeds from the chip instead of writing the vendor's configured
    //    initial values, which would drive `pwr_off` low -- powering up a board
    //    somebody deliberately switched off, merely by asking about it.
    assert!(
        core.contains("b.shadow = b.read_pins().unwrap_or(0);"),
        "the open path must seed its shadow from READ_PINS"
    );

    // 3. Questions take no claim and set no mode. SET_BITMODE reloads the output
    //    latch, so a `power-state` that configured the chip would drop the very
    //    level it was asked about.
    let questions = hook
        .split("Everything past here DRIVES the board")
        .next()
        .expect("the actuation boundary comment must exist");
    // …and a question must be answered BEFORE the action vocabulary is
    // consulted. Parsing `id` as a boot action rejected both read-only
    // subcommands with "unknown TAC action" -- measured on the bravo host, where
    // the read-only probe was unreachable through the path built for it.
    assert!(
        questions.find("if action == \"id\"").unwrap_or(usize::MAX)
            < questions.find("let act = match action").unwrap_or(0),
        "id/power-state must be handled before the action is parsed"
    );
    assert!(
        !questions.contains("Tac::open_for"),
        "id/power-state/dry-run must not claim the GPIO channels"
    );
    assert!(
        questions.contains("TacProbe::open"),
        "questions must go through the read-only probe"
    );
    // 4. The MEASUREMENT is the answer, and it comes first: conminer parses the
    //    first token of power-state by equality. Printing the commanded value
    //    there is how a commanded-only controller reports "on" for a board that
    //    never came up. Measured on the IQ8: `md_resout` reads 1 running and 0
    //    off, which makes this the rare strap controller that can measure.
    let ps = hook
        .split("if action == \"power-state\"")
        .nth(1)
        .and_then(|t| t.split("return Ok(())").next())
        .expect("the power-state branch");
    assert!(
        ps.contains("profile\n            .sense") || ps.contains("profile.sense"),
        "power-state must consult the profile's sense pin"
    );
    let sensed_first = ps
        .find("(Some(s), Some(c))")
        .expect("the sensed+commanded arm");
    let commanded_only = ps
        .find("(None, Some(c))")
        .expect("the no-sense-line arm must exist and say unknown");
    assert!(
        sensed_first < commanded_only,
        "the sensed answer must be reached before the commanded fallback"
    );
    assert!(
        ps[commanded_only..].contains("unknown"),
        "with no sense line the honest answer is unknown, not the command"
    );

    let probe = core
        .split("impl TacProbe {")
        .nth(1)
        .and_then(|t| t.split("\n}\n").next())
        .expect("TacProbe impl");
    for forbidden in ["bitmode(", "bulk_out(", "claim_interface"] {
        assert!(
            !probe.contains(forbidden),
            "the read-only probe must not {forbidden}: it would disturb a board \
             that was only being asked a question"
        );
    }
}

/// discoveryd is the safety net that reattaches ftdi_sio when a tool detaches
/// it, which is a usbfs ioctl rather than a tty write. Measured: after a
/// power-off the Bughopper's FTDI sat on the bus with driver=NONE and the
/// console stayed missing, because the recovery could not open the usbfs node.
#[test]
fn services_that_recover_consoles_can_reach_usbfs() {
    let compose = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docker-compose.yaml"
    ))
    .expect("docker-compose.yaml");

    // Both the service that runs power hooks and the one that runs the safety
    // net need usbfs (char major 189).
    let usbfs = compose.matches("'c 189:* rmw'").count();
    assert!(
        usbfs >= 2,
        "usbfs must be granted to both mcpd and discoveryd, found {usbfs}"
    );
}

/// Wedge handling, end to end. conminer is the lowest layer on the bench: if it
/// reports a state the hardware is not in, everything above inherits the lie.
/// Three wedges were found by stress testing on real boards, and each needs a
/// detection AND a response.
#[test]
fn every_known_wedge_is_detected_and_answered() {
    let tools = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    let live = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-core/src/live.rs"
    ))
    .expect("live.rs");
    let svc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/service.rs"))
        .expect("service.rs");

    // 1. Board wedge: a reset opens an epoch and captures nothing. Detected by
    //    checking the effect, answered by escalating to a power cycle.
    assert!(
        tools.contains("fn verify_power_effect"),
        "power effect must be verified"
    );
    assert!(
        tools.contains(r#""reset" | "on" => Some("cycle")"#),
        "a dead reset must escalate"
    );

    // 2. Capture wedge: the read loop failing the same way forever while the
    //    device still advertises itself as attached.
    assert!(
        live.contains("same_error >= 3"),
        "repeated identical failure must be detected"
    );
    assert!(
        live.contains("self.set_state(CaptureState::NotListening);"),
        "a wedged capture must stop claiming to be listening"
    );

    // 3. ser2net wedge: a reload that leaves no accepters while sessions
    //    survive, so capture looks healthy and new attaches are refused.
    assert!(
        svc.contains("fn missing_listeners"),
        "listener loss must be detected"
    );
    assert!(
        svc.contains("COOLDOWN_TICKS"),
        "and recovery must not become a restart loop"
    );
}

/// A rebuild restarts every service. Doing that while a hardware run is in
/// flight corrupts it: an EDL stress cycle was recorded as a double hook
/// failure when mcpd went down underneath it, which reads as a broken board
/// rather than a broken test.
#[test]
fn deploying_refuses_while_a_hardware_run_holds_the_bench() {
    let cm = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cm")).expect("cm");

    assert!(cm.contains("require_bench_free"), "no bench guard exists");
    // The two commands that restart services must both be guarded.
    for cmd in ["build)   require_bench_free", "up)      require_bench_free"] {
        assert!(cm.contains(cmd), "unguarded deploy path: {cmd}");
    }
    // A dead owner's lock must be breakable, or one crash wedges the bench --
    // the same rule the power hooks learned.
    assert!(
        cm.contains("kill -0 \"$owner\""),
        "stale locks must be detected"
    );
    assert!(
        cm.contains("rm -rf \"$BENCH_LOCK\"   # dead owner"),
        "and broken"
    );
    // And a run must release the lock on every exit path, including a signal.
    assert!(
        cm.contains(r#"trap 'rm -rf "$BENCH_LOCK"' EXIT INT TERM"#),
        "the lock must not outlive its run"
    );
}

/// §P1. peerd is the ONLY service on the host's network, and it must be.
///
/// Multicast and broadcast do not cross a docker bridge, so a beacon on the
/// bridge network reaches nobody. The fix is one service with `network_mode:
/// host` -- and only one: giving mcpd or minerd the host's stack to save a
/// container would hand it to the processes that drive boards and hold the
/// stores, for the sake of a postcard-sized advert.
#[test]
fn only_peerd_takes_the_hosts_network() {
    let c = compose();
    let mut host_net = Vec::new();
    let mut current = String::new();
    for line in c.lines() {
        // Service keys sit at exactly two spaces of indent.
        if line.len() > 2
            && line.starts_with("  ")
            && !line.starts_with("   ")
            && line.trim_end().ends_with(':')
        {
            current = line.trim().trim_end_matches(':').to_string();
        }
        if line.contains("network_mode:") && line.contains("host") {
            host_net.push(current.clone());
        }
    }
    assert_eq!(
        host_net,
        vec!["peerd".to_string()],
        "exactly one service may take the host network, and it is the one that only sends \
         adverts"
    );

    // And it must carry the data volume: the peer table and the remote device
    // rows live in the registry every other service reads.
    let mut peerd = String::new();
    let mut in_peerd = false;
    for line in c.lines() {
        let is_service_key =
            line.starts_with("  ") && !line.starts_with("   ") && line.trim_end().ends_with(':');
        if is_service_key {
            in_peerd = line.trim() == "peerd:";
            continue;
        }
        if in_peerd {
            peerd.push_str(line);
            peerd.push('\n');
        }
    }
    assert!(
        peerd.contains("conminer-data"),
        "peerd writes the peer table and the remote rows; without the shared volume mcpd and \
         dashd would never see them: {peerd}"
    );
}

// ------------------------------------------------ actuation on real controllers

/// A BOARD IS ALIVE IF ANY OF ITS CONSOLES SPEAKS.
///
/// An EVK exposes four interfaces and usually talks on exactly one. Aim a power
/// action at either of the others -- which the dashboard's own panel does, and
/// which any agent may do -- and verification watched a port that was never
/// going to answer: it waited out the whole boot window, declared
/// `verified: false` for a board sitting at a prompt, and then POWER-CYCLED it
/// to "recover" it. Measured on the hardware matrix at 74 s per press, with an
/// unrequested second cycle every time.
///
/// The same defect was found and fixed for the `target` form when target
/// actuation shipped; the `device` form kept passing a single console.
#[test]
fn verification_watches_every_console_of_the_board_not_just_the_one_named() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    // The scope carries a set for "who proves it responded", separate from
    // "whose epoch this opens" -- widening the latter would strand evidence.
    assert!(
        src.contains("watched: Vec<DeviceRow>"),
        "the actuation scope must carry a watched set"
    );
    assert!(
        src.contains("fn board_siblings("),
        "…resolved from the board, not the console"
    );
    // Whitespace-insensitive: the call spans several lines once it takes more
    // arguments, and an assertion that pins the formatting fails on a rustfmt
    // pass that changed nothing about the behaviour it guards.
    let flat: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("verify_power_effect( ctx, d, &scope.watched,"),
        "…and verification must use it"
    );
    // The device form must populate it; a single-console watch is the bug.
    let device_form = src
        .split("let watched = board_siblings(ctx, &d);")
        .nth(1)
        .and_then(|t| t.split("});").next())
        .expect("the device form builds a scope");
    assert!(
        device_form.contains("watched,"),
        "the device form must carry the board's consoles: {device_form}"
    );
}

// The per-controller serialisation this deployment depends on is asserted
// BEHAVIOURALLY in `conminer_core::hooks::tests` -- two hooks actually run
// against one key and their intervals must not overlap, three concurrent probes
// must cost one run, and two keys must map to two locks. A source-text gate
// lived here first; it passed with the locking bypassed, and then broke on a
// refactor that changed nothing about the behaviour. A duplicate that can be
// both wrong and brittle is worth less than the test it duplicates.

// ------------------------------------------- a deploy must not rename a node -

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

/// A DEPLOY MUST NOT OVERWRITE WHAT THE NODE OWNS.
///
/// `.env` carries `CONMINER_PEERS_NAME` and the advertise host: it is the
/// node's identity, not the project's. A sync that copied one node's `.env` onto
/// the others renamed three hosts to a single name in one deploy. Every peer
/// table then listed what looked like duplicate registrations, and the router
/// had two candidates for every actuation -- on a bench where routing decides
/// which board gets powered off.
///
/// Executed against the real `cm sync`, into a directory standing in for a node.
#[test]
fn syncing_the_source_leaves_the_nodes_identity_alone() {
    let node = tempfile::tempdir().unwrap();
    let identity = "CONMINER_PEERS_NAME=alpha\nCONMINER_PEERS_ADVERTISE_HOST=192.168.10.10\n";
    std::fs::write(node.path().join(".env"), identity).unwrap();
    // And something the node persisted that the source tree knows nothing about.
    std::fs::write(
        node.path().join("instance.json"),
        r#"{"instance_id":"abc"}"#,
    )
    .unwrap();

    let out = std::process::Command::new("bash")
        .arg("cm")
        .arg("sync")
        .arg("--local")
        .arg(node.path())
        .current_dir(repo_root())
        .output()
        .expect("cm sync runs");
    assert!(
        out.status.success(),
        "cm sync failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        std::fs::read_to_string(node.path().join(".env")).unwrap(),
        identity,
        "the node's identity must survive a deploy"
    );
    assert!(
        std::fs::read_to_string(node.path().join("instance.json"))
            .unwrap()
            .contains("abc"),
        "the node's persisted identity must survive a deploy"
    );
    // And the sync actually happened, or this proves nothing at all.
    assert!(
        node.path().join("crates/conminer/src/dash.rs").exists(),
        "the source never arrived"
    );
}

/// A stale file left behind would change the fingerprint while the source that
/// produced it is gone, leaving the node reporting a build nobody can rebuild.
#[test]
fn syncing_clears_stale_sources_so_the_fingerprint_is_honest() {
    let node = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(node.path().join("crates/ghost")).unwrap();
    std::fs::write(
        node.path().join("crates/ghost/lib.rs"),
        "// deleted upstream",
    )
    .unwrap();

    let sync = std::process::Command::new("bash")
        .args(["cm", "sync", "--local"])
        .arg(node.path())
        .current_dir(repo_root())
        .output()
        .expect("cm sync runs");
    assert!(sync.status.success());
    assert!(
        !node.path().join("crates/ghost/lib.rs").exists(),
        "a source file deleted upstream must not survive on the node"
    );

    // The proof that matters: the node computes the fingerprint this tree deploys as.
    let id_of = |dir: &std::path::Path| {
        let o = std::process::Command::new("bash")
            .args(["cm", "build-id"])
            .current_dir(dir)
            .output()
            .expect("cm build-id runs");
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let here = id_of(&repo_root());
    assert!(!here.is_empty(), "build-id produced nothing");
    assert_eq!(
        id_of(node.path()),
        here,
        "a synced node must fingerprint identically to the tree it was synced from"
    );
}

/// DEPLOYING MUST NOT ERASE THE BUILD FINGERPRINT.
///
/// `./cm image` passed `CONMINER_BUILD` as a `--build-arg`, but `./cm up` runs
/// `up -d --build`, which rebuilds from the compose `args` alone. Those args
/// did not carry it, so the Dockerfile default won and every service on the
/// node reported `build: unknown` -- measured on charlie straight after a
/// deploy. The fingerprint exists precisely because a Cargo version cannot
/// detect skew across the fleet, so a deploy path that erases it defeats the
/// only check there is.
#[test]
fn deploying_stamps_the_build_fingerprint_it_was_built_with() {
    let compose = compose();
    let build_block = compose
        .split("x-common:")
        .next()
        .expect("the image build block precedes the service anchors");
    // COMMENTS ARE NOT CONFIGURATION. Scanning the raw text made this gate pass
    // on the comment that EXPLAINS the arg: deleting the arg itself left the
    // docstring behind, and the fix's own prose satisfied the test for the fix.
    let build_args: String = build_block
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim_end())
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        build_args.contains("CONMINER_BUILD:"),
        "compose must pass CONMINER_BUILD into the image build, or `up --build` \
         silently reverts it to the Dockerfile default:\n{build_args}"
    );

    // ...and the deploy command must actually supply it, or the compose
    // default is all anyone ever gets.
    let cm = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../cm"))
        .expect("cm is the deploy entry point");
    let up = cm
        .split("  up)")
        .nth(1)
        .expect("cm must have an `up` subcommand")
        .split(";;")
        .next()
        .expect("the up branch");
    assert!(
        up.contains("CONMINER_BUILD=\"$(build_id)\""),
        "`cm up` must stamp the fingerprint of the tree it is deploying:\n{up}"
    );

    // The Dockerfile's fallback is what makes the failure silent, so it must
    // stay a recognisable sentinel rather than something that looks like a real
    // fingerprint.
    assert!(
        dockerfile().contains("ARG CONMINER_BUILD=unknown"),
        "an unstamped build must be obviously unstamped"
    );
}

/// ser2net's lock directory must be ephemeral.
///
/// `/run/lock` in this image is the container's writable layer, not a tmpfs, so
/// a UUCP lock written by one ser2net outlives it and is still there when the
/// container is RESTARTED rather than recreated. Seen after a host reboot:
/// locks four days old naming pid 16, a fresh ser2net that was
/// also pid 16, and every console pinned at open_failed because the lock looked
/// live. The supervisor clears them before each spawn; this keeps them from
/// being persisted at all, which is what `/run` is for.
#[test]
fn the_ser2net_lock_directory_is_a_tmpfs() {
    let compose = include_str!("../../../../docker-compose.yaml");
    let svc = compose
        .split("  ser2net:")
        .nth(1)
        .expect("the ser2net service")
        .split("\n  minerd:")
        .next()
        .expect("the end of it");
    assert!(
        svc.contains("tmpfs:") && svc.contains("/run/lock"),
        "ser2net must mount /run/lock as a tmpfs so a lock cannot outlive the \
         process that wrote it:\n{svc}"
    );
}
