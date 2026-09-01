//! Suite `config` (§16 "Tests (suite `config`)").
//!
//! "every key above has a default-applied test and an override test;
//!  exclude-glob device never opened (and listed `ignored`); commit-interval
//!  durability property; max_record_lines truncation flag; hook timeout →
//!  structured error; credentials never appear in store, export, or logs;
//!  check-config rejects unknown keys (typo protection) and out-of-range values
//!  with the offending line."
//!
//! The durability, truncation, hook and credential-scanner cases live in the
//! suites that own those subsystems (`store`, `framer-*`, `runner`); this suite
//! owns the parse/default/override/validation surface, and asserts that the
//! shipped `conminer.toml` is exactly the documented defaults.

use conminer_core::config::{Config, FlowControl, LineEndingMode, Parity};
use conminer_core::ErrorCode;

fn shipped() -> Config {
    let text = include_str!("../../../../conminer.toml");
    Config::from_toml_str(text).expect("the shipped conminer.toml must be valid")
}

// ------------------------------------------------------------- defaults ------

#[test]
fn shipped_config_equals_the_compiled_defaults() {
    // §16 is a *reference*: the file must state the defaults, not diverge from
    // them. If someone changes a default in code, this fails until the file and
    // the docs follow.
    assert_eq!(shipped(), Config::default());
}

#[test]
fn every_documented_default_is_applied_from_an_empty_file() {
    let c = Config::from_toml_str("").unwrap();

    // discovery & attachment
    assert_eq!(c.discovery.include, vec!["*".to_string()]);
    assert!(c.discovery.exclude.is_empty());
    assert_eq!(c.discovery.hotplug_debounce_ms, 500);
    assert_eq!(c.discovery.poll_fallback_hz, 1);
    assert_eq!(c.ser2net.base_port, 5001);
    assert_eq!(c.attach.reconnect_backoff_ms, 250);
    assert_eq!(c.attach.reconnect_backoff_max_ms, 15_000);
    assert_eq!(c.attach.tcp_keepalive_s, 10);

    // line (§3.2)
    assert_eq!(c.line.baud, 115_200);
    assert_eq!(c.line.data_bits, 8);
    assert_eq!(c.line.parity, Parity::None);
    assert_eq!(c.line.stop_bits, 1);
    assert_eq!(c.line.flow, FlowControl::None);
    assert_eq!(c.line.tx_line_ending, "\n");
    assert!(!c.line.auto_baud);

    // capture, sessions & durability
    assert_eq!(c.capture.ring_mb, 64);
    assert_eq!(c.capture.commit_interval_ms, 250);
    assert_eq!(c.capture.encoding, "raw");
    assert_eq!(c.capture.line_ending_mode, LineEndingMode::Auto);
    assert_eq!(c.session.autosplit_quiet_s, 300);
    assert_eq!(c.session.max_hours, 24);
    assert_eq!(c.retention.live_cap_gb, 2.0);
    assert_eq!(c.retention.file_sessions, "keep-all");

    // framing & mining
    assert_eq!(c.framer.lookback_lines, 64);
    assert_eq!(c.framer.max_record_lines, 2000);
    assert_eq!(c.framer.record_timeout_s, 10);
    assert_eq!(c.framer.garbage_threshold, 0.30);
    assert_eq!(c.framer.garbage_window_bytes, 512);
    assert_eq!(c.mine.similarity, 0.4);
    assert_eq!(c.mine.depth, 4);
    assert_eq!(c.mine.max_children, 100);
    assert_eq!(c.mine.max_line_tokens, 128);
    assert!(c.search.fts);

    // interaction
    assert_eq!(c.runner.char_delay_ms, 10);
    assert_eq!(c.runner.echo_timeout_ms, 200);
    assert_eq!(c.runner.command_timeout_s, 30);
    assert_eq!(c.runner.settle_quiet_ms, 500);
    assert_eq!(c.runner.escape_set, ["C-c", "C-\\", "C-d"]);
    assert!(
        !c.runner.allow_raw_send,
        "the send passthrough is off by default"
    );
    assert_eq!(c.state.hung_after_s, 30);
    assert_eq!(c.state.loop_min_epochs, 3);
    assert_eq!(c.lease.ttl_s, 900);
    assert_eq!(c.lease.max_s, 14_400);
    assert_eq!(c.hooks.power_timeout_s, 30);
    assert_eq!(c.hooks.flash_timeout_s, 600);
    assert_eq!(c.credentials.file, "", "credentials are unset by default");

    // service & api
    assert_eq!(c.mcpd.bind, "127.0.0.1", "§14.1: loopback by default");
    assert_eq!(c.mcpd.port, 8090);
    assert_eq!(c.api.max_raw_lines, 200);
    assert_eq!(c.api.max_results, 100);
    assert_eq!(c.api.follow_timeout_max_s, 600);
    assert_eq!(c.api.max_concurrent_follows, 64);
    assert_eq!(c.follow.default_timeout_s, 30);
    assert_eq!(c.notify.coalesce_ms, 5000);
    assert_eq!(c.export.max_gb, 4.0);
    assert_eq!(c.ingest.max_gb, 2.0);
    assert_eq!(c.ingest.gzip, "auto");
    assert_eq!(c.log.level, "info");
    assert_eq!(c.metrics.bind, "127.0.0.1:9090");
    assert_eq!(c.time.store, "utc");
}

// ------------------------------------------------------------- overrides -----

#[test]
fn every_section_can_be_overridden_from_file() {
    let c = Config::from_toml_str(
        r#"
        [discovery]
        include = ["usb-FTDI*"]
        exclude = ["*UPS*"]
        hotplug_debounce_ms = 1000
        poll_fallback_hz = 5

        [ser2net]
        base_port = 6001

        [attach]
        tcp_keepalive_s = 30

        [line]
        baud = 921600
        data_bits = 7
        parity = "even"
        flow = "rtscts"
        tx_line_ending = "\r\n"
        auto_baud = true

        [capture]
        ring_mb = 256
        commit_interval_ms = 0
        line_ending_mode = "cr"

        [session]
        autosplit_quiet_s = 60

        [retention]
        live_cap_gb = 10.0
        file_sessions = "30d"

        [framer]
        lookback_lines = 128
        max_record_lines = 500
        record_timeout_s = 45

        [mine]
        similarity = 0.55
        depth = 5
        max_children = 20
        max_line_tokens = 64

        [search]
        fts = false

        [runner]
        char_delay_ms = 50
        allow_raw_send = true
        escape_set = ["C-c", "~."]

        [state]
        hung_after_s = 120
        loop_min_epochs = 5

        [lease]
        ttl_s = 60
        max_s = 120

        [hooks]
        power_timeout_s = 5

        [mcpd]
        bind = "0.0.0.0"
        port = 9999

        [api]
        max_raw_lines = 10
        max_results = 5

        [notify]
        coalesce_ms = 100

        [ingest]
        max_gb = 8.0
        gzip = "off"

        [log]
        level = "debug"
        "#,
    )
    .unwrap();

    assert_eq!(c.discovery.include, ["usb-FTDI*"]);
    assert_eq!(c.discovery.hotplug_debounce_ms, 1000);
    assert_eq!(c.ser2net.base_port, 6001);
    assert_eq!(c.attach.tcp_keepalive_s, 30);
    assert_eq!(c.line.baud, 921_600);
    assert_eq!(c.line.parity, Parity::Even);
    assert_eq!(c.line.flow, FlowControl::RtsCts);
    assert_eq!(c.line.tx_line_ending, "\r\n");
    assert!(c.line.auto_baud);
    assert_eq!(c.capture.ring_mb, 256);
    assert_eq!(c.capture.commit_interval_ms, 0, "0 = per-line fsync");
    assert_eq!(c.capture.line_ending_mode, LineEndingMode::Cr);
    assert_eq!(c.session.autosplit_quiet_s, 60);
    assert_eq!(c.retention.file_sessions, "30d");
    assert_eq!(c.framer.max_record_lines, 500);
    assert_eq!(c.mine.similarity, 0.55);
    assert_eq!(c.mine.depth, 5);
    assert!(!c.search.fts);
    assert_eq!(c.runner.char_delay_ms, 50);
    assert!(c.runner.allow_raw_send);
    assert_eq!(c.runner.escape_set, ["C-c", "~."]);
    assert_eq!(c.state.hung_after_s, 120);
    assert_eq!(c.lease.ttl_s, 60);
    assert_eq!(c.mcpd.port, 9999);
    assert_eq!(c.api.max_results, 5);
    assert_eq!(c.ingest.gzip, "off");
    assert_eq!(c.log.level, "debug");
}

#[test]
fn per_device_overrides_beat_globals() {
    let c = Config::from_toml_str(
        r#"
        [line]
        baud = 115200

        [runner]
        char_delay_ms = 10

        [search]
        fts = true

        [state]
        hung_after_s = 30

        [devices."bl-console"]
        nickname = "bl-console"
        pinned_profile = "uboot"
        tags = { rack = "r2", role = "ap-console" }

        [devices."bl-console".line]
        baud = 1500000

        [devices."bl-console".runner]
        char_delay_ms = 40

        [devices."bl-console".search]
        fts = false

        [devices."bl-console".state]
        hung_after_s = 5

        [devices."bl-console".hooks]
        power = "pdu-ctl {action} --outlet 4"
        "#,
    )
    .unwrap();

    assert_eq!(c.line_for("bl-console").baud, 1_500_000);
    assert_eq!(c.line_for("anything-else").baud, 115_200);
    assert_eq!(c.runner_for("bl-console").char_delay_ms, 40);
    assert_eq!(c.runner_for("other").char_delay_ms, 10);
    assert!(!c.fts_for("bl-console"));
    assert!(c.fts_for("other"));
    assert_eq!(c.hung_after_s_for(&["bl-console"]), 5);
    assert_eq!(c.hung_after_s_for(&["other"]), 30);

    let dev = &c.devices["bl-console"];
    assert_eq!(dev.pinned_profile.as_deref(), Some("uboot"));
    assert_eq!(dev.tags["rack"], "r2");
    assert_eq!(
        dev.hooks.power.as_deref(),
        Some("pdu-ctl {action} --outlet 4")
    );
}

#[test]
fn env_overrides_beat_the_file() {
    let mut c = Config::from_toml_str("[mcpd]\nport = 8090\n").unwrap();
    c.apply_env(&[
        ("CONMINER_MCPD_PORT".into(), "7000".into()),
        ("CONMINER_MCPD_BIND".into(), "0.0.0.0".into()),
        ("CONMINER_DATA".into(), "/data".into()),
        ("CONMINER_SEARCH_FTS".into(), "false".into()),
    ]);
    assert_eq!(c.mcpd.port, 7000);
    assert_eq!(c.mcpd.bind, "0.0.0.0");
    assert_eq!(c.paths.data_dir.to_str().unwrap(), "/data");
    assert!(!c.search.fts);
}

// ------------------------------------------------------ exclude-glob gate ----

#[test]
fn exclude_glob_device_is_never_opened() {
    let c = Config::from_toml_str(
        r#"
        [discovery]
        include = ["*"]
        exclude = ["*Quectel*", "*_UPS_*", "usb-SEGGER_J-Link*if02*"]
        "#,
    )
    .unwrap();

    // the boards we own
    assert!(c.device_included("usb-FTDI_TTL232R-3V3_FTB6SPL3-if00-port0"));
    assert!(c.device_included("usb-SEGGER_J-Link_000123456789-if00"));

    // the lab furniture conminer must stay off
    assert!(!c.device_included("usb-Quectel_RM520N-GL_Modem-if02"));
    assert!(!c.device_included("usb-APC_UPS_serial-if00"));
    assert!(!c.device_included("usb-SEGGER_J-Link_000123456789-if02-port0"));
}

#[test]
fn an_empty_include_list_opens_nothing() {
    let c = Config::from_toml_str("[discovery]\ninclude = []\n").unwrap();
    assert!(!c.device_included("usb-FTDI_anything"));
}

// -------------------------------------------------------- check-config -------

#[test]
fn unknown_key_is_rejected_with_the_offending_line() {
    let err = Config::from_toml_str("[mine]\nsimilarity = 0.4\n\n[runner]\nchar_delay_msec = 10\n")
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidConfig);
    let d = err.detail.expect("the offending line must be attached");
    assert_eq!(d["line"], 5);
    assert!(d["text"].as_str().unwrap().contains("char_delay_msec"));
}

#[test]
fn unknown_section_is_rejected() {
    let err = Config::from_toml_str("[minerd]\nthreads = 4\n").unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidConfig);
}

#[test]
fn out_of_range_values_are_rejected_by_key() {
    let cases: &[(&str, &str)] = &[
        ("[mine]\nsimilarity = 1.5\n", "mine.similarity"),
        ("[mine]\ndepth = 2\n", "mine.depth"),
        ("[mine]\nmax_children = 0\n", "mine.max_children"),
        (
            "[framer]\ngarbage_threshold = 30.0\n",
            "framer.garbage_threshold",
        ),
        ("[capture]\nmax_line_bytes = 8\n", "capture.max_line_bytes"),
        ("[ser2net]\nbase_port = 80\n", "ser2net.base_port"),
        ("[api]\nmax_results = 0\n", "api.max_raw_lines"),
        ("[log]\nlevel = \"chatty\"\n", "log.level"),
        ("[line]\nstop_bits = 3\n", "line.stop_bits"),
        ("[line]\ndata_bits = 9\n", "line.data_bits"),
        (
            "[retention]\nfile_sessions = \"forever\"\n",
            "retention.file_sessions",
        ),
        ("[ingest]\ngzip = \"maybe\"\n", "ingest.gzip"),
    ];
    for (toml, key) in cases {
        let err = Config::from_toml_str(toml).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidConfig, "{toml}");
        assert!(
            err.message.contains(key),
            "expected {key} in {:?}",
            err.message
        );
    }
}

#[test]
fn cross_field_consistency_is_checked() {
    let err = Config::from_toml_str("[lease]\nttl_s = 100\nmax_s = 50\n").unwrap_err();
    assert!(err.message.contains("lease.ttl_s"));

    let err = Config::from_toml_str(
        "[api]\nfollow_timeout_max_s = 10\n\n[follow]\ndefault_timeout_s = 60\n",
    )
    .unwrap_err();
    assert!(err.message.contains("follow.default_timeout_s"));
}

#[test]
fn all_problems_are_reported_not_just_the_first() {
    let problems = Config::check("[mine]\nsimilarity = 9.0\ndepth = 1\nmax_children = 0\n")
        .expect("parses; only the ranges are wrong");
    assert_eq!(problems.len(), 3);
    let keys: Vec<&str> = problems.iter().map(|p| p.key.as_str()).collect();
    assert!(keys.contains(&"mine.similarity"));
    assert!(keys.contains(&"mine.depth"));
    assert!(keys.contains(&"mine.max_children"));
}

#[test]
fn invalid_device_regexes_are_caught_at_config_time_not_at_runtime() {
    let err = Config::from_toml_str(
        r#"
        [devices."board"]
        prompts = ["=> ", "([unclosed"]
        "#,
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidConfig);
    assert!(err.message.contains("devices.board.prompts"));
}

#[test]
fn invalid_discovery_globs_are_caught_at_config_time() {
    let err = Config::from_toml_str("[discovery]\ninclude = [\"usb-[\"]\n").unwrap_err();
    assert!(err.message.contains("discovery.include"));
}

/// A controller that carries its own console must not be excluded from
/// discovery. `exclude_from_discovery` defaults to TRUE, so a new profile
/// silently deletes the very port it is meant to serve unless it says
/// otherwise -- which is the trap the Bughopper profile walked into.
#[test]
fn a_controller_that_is_also_a_console_stays_discoverable() {
    let cfg = Config::default();
    let bh = cfg
        .controllers
        .iter()
        .find(|c| c.name == "bughopper")
        .expect("bughopper profile missing");

    assert!(
        !bh.exclude_from_discovery,
        "the Bughopper's FTDI IS the console; excluding it deletes the port"
    );
    assert!(
        bh.power.is_some(),
        "a controller must be able to power the board"
    );
    // It controls the board it lives on.
    assert_eq!(bh.match_glob, "*Bughopper*");
    assert_eq!(bh.controls, "*Bughopper*");

    // The Bantam is the opposite case and must stay excluded: it is control-only,
    // and ser2net holding it would deny the controller clean command access.
    let bantam = cfg.controllers.iter().find(|c| c.name == "bantam").unwrap();
    assert!(bantam.exclude_from_discovery, "the Bantam is control-only");
}

/// The same trap, for the TAC: one FT4232H is BOTH the board's two consoles and
/// the GPIO that powers it. `exclude_from_discovery` defaults to true, so
/// getting this wrong would make a working controller delete the pair of UARTs
/// it exists to serve -- and on the bravo node those two UARTs are the entire
/// reason the host is on the bench.
#[test]
fn a_tac_controller_does_not_exclude_its_own_uarts() {
    let cfg = Config::default();
    let tac = cfg
        .controllers
        .iter()
        .find(|c| c.name == "tac")
        .expect("tac profile missing");
    assert!(!tac.exclude_from_discovery);

    // The real by-id names from the bravo host.
    for console in [
        "usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if00-port0",
        "usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if01-port0",
    ] {
        assert!(
            cfg.device_included(console),
            "{console} was excluded by its own controller profile"
        );
        assert_eq!(
            cfg.controller_for(console).map(|c| c.name.as_str()),
            Some("tac"),
            "{console} must resolve the TAC, not the bantam catch-all"
        );
    }

    // A freshly booted host has ftdi_sio on ALL FOUR channels, so the two GPIO
    // ones show up in /dev/serial/by-id looking exactly like consoles. They are
    // the board's power and strap lines, and opening a tty asserts DTR and RTS.
    for gpio in [
        "usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if02-port0",
        "usb-FTDI_RIDE_MICRO_4.0_FTAFIF1Q-if03-port0",
    ] {
        assert!(
            !cfg.device_included(gpio),
            "{gpio} is a bit-bang GPIO port, not a console: serving it to ser2net \
             writes to the board's power lines"
        );
    }

    // It drives the board through the console's own by-id path, so it needs
    // nothing else plugged in -- and therefore must never carry `{controller}`,
    // which would make the hook refuse for want of a second device.
    let power = tac.power.as_deref().unwrap();
    assert!(power.contains("{device}"), "{power}");
    assert!(
        !power.contains("{controller}"),
        "a TAC has no separate controller node to resolve: {power}"
    );
    assert!(
        tac.mode_enters_immediately,
        "every TAC mode sequence powers the board up inside the strap window: \
         a reset afterwards boots it straight back out"
    );
}

/// A bench with TWO boards of the same controller family must bind each board's
/// controller to its OWN consoles. Both a NordAU RIDE SX and an IQ10 are driven
/// by a Bantam ("Karussell") with an identical command set, so the by-id name
/// cannot tell them apart -- and picking the first match would power-cycle the
/// wrong board. USB topology decides: devices sharing a downstream hub are one
/// harness.
#[test]
fn two_boards_of_the_same_family_bind_their_own_controllers() {
    use conminer_core::config::topology_group;

    // Real paths from the bench: IQ10 at 3.2.x, NordAU RIDE SX at 3.1.x.
    let iq10_uart = "pci-0000:00:14.0-usb-0:3.2.2:1.2-port0";
    let iq10_bantam = "pci-0000:00:14.0-usb-0:3.2.4:1.0";
    let ride_uart = "pci-0000:00:14.0-usb-0:3.1.2:1.2-port0";
    let ride_bantam = "pci-0000:00:14.0-usb-0:3.1.4:1.0";

    assert_eq!(
        topology_group(Some(iq10_uart)),
        topology_group(Some(iq10_bantam))
    );
    assert_eq!(
        topology_group(Some(ride_uart)),
        topology_group(Some(ride_bantam))
    );
    assert_ne!(
        topology_group(Some(iq10_uart)),
        topology_group(Some(ride_uart)),
        "two boards must not share a group, or power goes to the wrong one"
    );

    let cfg = Config::default();
    let present = [
        (
            "usb-Microchip_Technology_Inc._Bantam_IQ10RRDXX-if00",
            Some(iq10_bantam),
        ),
        (
            "usb-Microchip_Technology_Inc._Bantam_KARUSSELLXX-if00",
            Some(ride_bantam),
        ),
    ];

    // The RIDE's console must resolve to the RIDE's Bantam, not the IQ10's.
    let picked = cfg
        .controller_port_for(
            "usb-FTDI_NordAU_RIDE_SX_879X_UART_AI41BI4U0R-if02-port0",
            Some(ride_uart),
            present.iter().copied(),
        )
        .expect("the RIDE board should resolve a controller");
    assert!(
        picked.contains("KARUSSELL"),
        "bound the wrong board's controller: {picked}"
    );

    // And the IQ10's console to the IQ10's.
    let picked = cfg
        .controller_port_for(
            "usb-FTDI_IQ10_UART-SPI_AR40BYP4AU-if02-port0",
            Some(iq10_uart),
            present.iter().copied(),
        )
        .expect("the IQ10 should resolve a controller");
    assert!(
        picked.contains("IQ10RRD"),
        "bound the wrong board's controller: {picked}"
    );
}

/// A newly plugged board must get power control with NO configuration. The
/// bantam profile's `controls` glob was `*IQ10*`, so a NordAU RIDE SX arrived
/// with six live consoles and no way to power them.
#[test]
fn a_new_board_of_a_known_family_gets_power_with_no_config() {
    let cfg = Config::default();
    let ride = "usb-FTDI_NordAU_RIDE_SX_879X_UART_AI41BI4U0R-if02-port0";
    let c = cfg
        .controller_for(ride)
        .expect("a Bantam-driven board must resolve a controller profile");
    assert_eq!(c.name, "bantam");
    // The full Karussell sequence set, confirmed against the live controller.
    for mode in [
        "BOOT_MD_EDL",
        "BOOT_SS_EDL",
        "BOOT_UEFI",
        "MD_FASTBOOT",
        "SS_MD_FASTBOOT",
    ] {
        assert!(
            c.boot_modes.iter().any(|m| m == mode),
            "missing boot mode {mode}"
        );
    }
}

/// The by-id product string is NOT authoritative about what a port carries.
/// Two boards have now taught this: the IQ10's "UART-SPI" adapter carries two
/// SPI channels on if00/if01, and the NordAU RIDE SX's four "UART" ports are an
/// AP console, a SAIL console, a dead port and an independently powered safety
/// monitor -- in that order. Anything that assumes "if02 is the AP console"
/// because it was on one board will silently watch the wrong port: stressing
/// the RIDE against its monitor reported "no boot output" and "never goes
/// quiet" while the board was booting perfectly.
#[test]
fn a_port_index_does_not_imply_what_the_port_carries() {
    let cfg = Config::default();

    // No profile may hard-code a console index as "the AP console".
    for c in &cfg.controllers {
        assert!(
            !c.controls.contains("if0") && !c.controls.contains("if1"),
            "{} binds by port index; that is board-specific and will mislead",
            c.name
        );
    }

    // Discovery must serve every port and let evidence decide which is which,
    // rather than filtering to a guessed console.
    assert!(
        cfg.discovery.include.iter().any(|g| g == "*"),
        "discovery must offer every port; which one is the AP console is measured, not assumed"
    );
}

/// A power hook that cannot name its controller must NOT be handed back.
///
/// THE INCIDENT: pressing power-off on the Bughopper board in the dashboard
/// powered off the IQ10, and answered ok:true verified:true. The topology check
/// correctly refused to bind a controller from another USB branch, but the hook
/// was returned anyway with controller: None. The command then ran with no
/// `{controller}` substituted, the shell script fell back to its default port
/// (/dev/ttyACM0), and that was a different board's controller -- which really
/// did read back PWR_OFF=1, so the action "verified".
///
/// A template containing `{controller}` states that it cannot act without
/// knowing which controller. Unresolved controller + such a template = no hook.
#[test]
fn a_power_hook_needing_a_controller_is_refused_when_none_can_be_resolved() {
    let cfg = Config::default();

    let console_b = "/dev/serial/by-id/usb-VendorY_BoardB_UART_BBBB-if00-port0";
    let path_b = "pci-0000:00:14.0-usb-0:3.3.1:1.0";
    // The only controller present sits on a DIFFERENT branch.
    let present = [(
        "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_ELSEWHERE-if00",
        Some("pci-0000:00:14.0-usb-0:1.2.2:1.0"),
    )];

    let hook = cfg.power_hook_for_at(console_b, console_b, Some(path_b), present);

    match hook {
        None => {} // correct: refuse
        Some(h) => {
            assert!(
                !h.template.contains("{controller}") || h.controller.is_some(),
                "returned a hook that needs a controller without one: template={:?} \
                 controller={:?} -- this is the cross-board actuation bug",
                h.template,
                h.controller
            );
        }
    }
}

/// The guard must not become an outage: a board WITH its own controller on its
/// own branch still resolves a hook, aimed at that controller.
#[test]
fn a_board_with_its_own_controller_still_resolves_an_aimed_hook() {
    let cfg = Config::default();

    let console = "/dev/serial/by-id/usb-FTDI_IQ10_UART-SPI_AAAA-if00-port0";
    let path = "pci-0000:00:14.0-usb-0:1.2.1:1.0";
    let ctrl = "/dev/serial/by-id/usb-Microchip_Technology_Inc._Bantam_MINE-if00";
    let present = [(ctrl, Some("pci-0000:00:14.0-usb-0:1.2.2:1.0"))];

    let hook = cfg
        .power_hook_for_at(console, console, Some(path), present)
        .expect("a board with its own controller must still get a power hook");

    if hook.template.contains("{controller}") {
        assert_eq!(
            hook.controller.as_deref(),
            Some(ctrl),
            "the hook must be aimed at THIS board's controller"
        );
    }
}

/// Every controller template MUST name the controller it drives.
///
/// THE ACTUAL DEFECT behind the cross-board actuation: the bantam templates were
///     "bantam-power {action} --settle {off_settle}"
/// with no `{controller}` anywhere. Topology resolved the right controller and
/// the template then threw it away, so the hook fell back to its own default
/// port and drove whichever board sat there. Proven on hardware: a power-off
/// aimed at the RIDE took down the IQ10 (RIDE PS_HOLD stayed 1, IQ10 1->0) and
/// still answered ok:true verified:true.
///
/// Hooks are argv-exec'd with no shell, so an env prefix cannot carry it either:
/// the port has to be an argument. Any controller whose hook targets a separate
/// controller device must therefore mention {controller}; controllers that drive
/// the board through the console device itself use {device} instead.
#[test]
fn every_controller_template_names_the_device_it_actuates() {
    let cfg = Config::default();
    for c in &cfg.controllers {
        for (what, tmpl) in [
            ("power", c.power.as_ref()),
            ("boot_mode", c.boot_mode.as_ref()),
            ("flash", c.flash.as_ref()),
        ] {
            let Some(t) = tmpl else { continue };
            assert!(
                t.contains("{controller}") || t.contains("{device}"),
                "controller {:?} {what} template does not say WHICH board it drives: {t:?} \
                 -- this is how a power-off aimed at one board hit another",
                c.name
            );
        }
    }
}

/// Specifically pin the bantam templates, because that is the pair that caused
/// the incident and a future edit could quietly drop the port again.
#[test]
fn the_bantam_templates_pass_the_controller_port() {
    let cfg = Config::default();
    let b = cfg
        .controllers
        .iter()
        .find(|c| c.name == "bantam")
        .expect("bantam profile");
    assert!(
        b.power
            .as_deref()
            .unwrap_or_default()
            .contains("--port {controller}"),
        "bantam power must pass --port {{controller}}: {:?}",
        b.power
    );
    assert!(
        b.boot_mode
            .as_deref()
            .unwrap_or_default()
            .contains("--port {controller}"),
        "bantam boot_mode must pass --port {{controller}}: {:?}",
        b.boot_mode
    );
}

/// A catch-all profile must never outrank a specific one.
///
/// `bantam` uses `controls = "*"` on purpose, so a newly plugged board of that
/// family gets power with zero config. But first-match let it claim the
/// Bughopper board, whose own FTDI drives power over CBUS and which has no
/// Bantam on its USB branch at all -- so the dashboard showed a power button
/// that could not work. Most specific glob wins.
#[test]
fn a_specific_controller_profile_beats_the_catch_all() {
    let cfg = Config::default();

    let bughopper = "/dev/serial/by-id/usb-Arduino_Bughopper_DK0HDSRI-if00-port0";
    let picked = cfg.controller_for(bughopper).expect("a profile must match");
    assert_eq!(
        picked.name, "bughopper",
        "the Bughopper board must bind its OWN controller, not the catch-all; got {:?}",
        picked.name
    );

    // And the catch-all still covers a board that has nothing more specific.
    let ride = "/dev/serial/by-id/usb-FTDI_NordAU_RIDE_SX_879X_UART_AI41BI4U0R-if00-port0";
    assert_eq!(
        cfg.controller_for(ride).map(|c| c.name.as_str()),
        Some("bantam"),
        "the catch-all must still give a new board of that family power with no config"
    );
}

/// A controller's hooks may declare how long they take.
///
/// FROM THE CROSS-PLATFORM REPORT (T1): the Bughopper claims a USB interface,
/// holds PM_RESIN_N for 6s and settles -- ~35s wall, past the 30s global
/// default. So `power off` and `cycle` on that board ALWAYS returned
/// HOOK_TIMEOUT, sometimes after actuating and sometimes without actuating at
/// all, while the Bantam hooks (1.7-5.4s) were unaffected. Hook duration is a
/// property of the controller, not of the deployment.
#[test]
fn a_slow_controller_declares_its_own_hook_timeout() {
    let cfg = Config::default();
    let bug = cfg
        .controllers
        .iter()
        .find(|c| c.name == "bughopper")
        .expect("bughopper profile");
    let t = bug
        .power_timeout_s
        .expect("bughopper must declare a timeout");
    assert!(
        t >= 45,
        "must exceed the ~35s the off sequence really takes, got {t}"
    );
    assert!(
        t > cfg.hooks.power_timeout_s,
        "otherwise it changes nothing"
    );

    // A fast controller stays on the global default rather than inventing one.
    let bantam = cfg
        .controllers
        .iter()
        .find(|c| c.name == "bantam")
        .expect("bantam profile");
    assert_eq!(bantam.power_timeout_s, None);
}

/// Anyone who can SET a boot strap must be able to RELEASE it the same way.
///
/// FROM THE REPORT (T3): on strap-latching controllers `boot_mode` only arms the
/// strap and nothing un-arms it, so every subsequent boot lands in EDL. The only
/// escape was reaching into the container for `bantam-power set MD_EDL 0`.
#[test]
fn every_strap_latching_controller_can_clear_its_straps() {
    let hook = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tools/bantam-power"
    ))
    .expect("bantam-power");
    assert!(
        hook.contains(r#"[ "$a1" = "clear" ]"#),
        "mode clear must exist"
    );
    for strap in ["MD_EDL", "SS_EDL", "UEFI", "FASTBOOT_MD"] {
        assert!(
            hook.contains(strap),
            "clearing must cover every strap it can set; missing {strap}"
        );
    }

    let tools = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/tools.rs"
    ))
    .expect("tools.rs");
    assert!(
        tools.contains(r#"mode.eq_ignore_ascii_case("clear")"#),
        "boot_mode must accept `clear` without it being a configured mode"
    );
}
