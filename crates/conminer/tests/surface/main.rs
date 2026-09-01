//! Coverage of the whole callable surface: MCP tools and dashboard API.
//!
//! These are deliberately MECHANICAL. A tool or route added tomorrow is covered
//! by them without anyone remembering to write a test, which is the only kind of
//! coverage rule that survives contact with a busy week.

use conminer_mcp::tools;
use serde_json::Value;

/// Every registered tool must be advertised under the shipped config.
///
/// S1 FROM THE SWEEP: `tools/list` advertised 19 of 69 tools. The other 50 --
/// including `decode`, `template_detail`, `get_records`, baselines, watches,
/// bisect, evidence, expectations and `evaluate_policy` -- were callable only by
/// hand-built HTTP, because an MCP client binds what `tools/list` returns and
/// cannot call anything else. They were, in effect, absent.
#[test]
fn every_registered_tool_is_advertised_by_default() {
    let cfg = conminer_core::config::Config::default();
    let advertised = tools::advertise_profile(cfg.api.full_toolset);
    let names: Vec<String> = advertised["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();

    let missing: Vec<&str> = tools::registry()
        .iter()
        .map(|t| t.name)
        .filter(|n| !names.iter().any(|a| a == n))
        .collect();
    assert!(
        missing.is_empty(),
        "these tools exist but no MCP client can call them: {missing:?}"
    );
}

/// Every tool must name its REQUIRED arguments in its own description.
///
/// S2 FROM THE SWEEP: guessing arguments from descriptions failed on seven
/// tools, because the prose said one thing and the schema another --
/// `get_records` "limit" was really `n`, `create_watch` wanted `name`/`until`
/// rather than a pattern, `bisect_report` took `candidate`, not `build`. Each
/// cost an agent a round-trip to discover. A description that omits its own
/// required arguments is incomplete documentation.
#[test]
fn every_tool_description_names_its_required_arguments() {
    let mut offenders: Vec<String> = Vec::new();
    for t in tools::registry() {
        let schema = (t.schema)();
        let Some(required) = schema.get("required").and_then(Value::as_array) else {
            continue;
        };
        // Check what an agent ACTUALLY SEES, which is the advertised
        // description, not the raw literal in the registry.
        let desc = tools::described(t).to_ascii_lowercase();
        for r in required {
            let Some(arg) = r.as_str() else { continue };
            // `device` is the universal selector, documented once globally.
            if arg == "device" {
                continue;
            }
            if !desc.contains(&arg.to_ascii_lowercase()) {
                offenders.push(format!("{} requires `{arg}` but never names it", t.name));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "an agent has to guess these, and guessing costs a round-trip each:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn every_advertised_tool_resolves_in_the_registry() {
    for t in tools::registry() {
        assert!(
            tools::find(t.name).is_some(),
            "{} is registered but not findable by name",
            t.name
        );
    }
}

#[test]
fn every_tool_has_a_usable_schema() {
    for t in tools::registry() {
        let s = (t.schema)();
        assert_eq!(s["type"], "object", "{} schema must be an object", t.name);
        assert!(
            s.get("properties").is_some(),
            "{} schema has no properties",
            t.name
        );
        assert!(
            !t.description.trim().is_empty(),
            "{} has no description",
            t.name
        );
    }
}

/// Mutating tools must be marked, so a caller can tell a question from an action
/// BEFORE taking one.
#[test]
fn tools_that_change_the_world_say_so() {
    for name in ["power", "boot_mode", "annotate_template"] {
        if let Some(t) = tools::find(name) {
            assert!(
                t.mutating,
                "{name} changes state and must be marked mutating"
            );
        }
    }
    for name in ["list_templates", "get_records", "diagnose", "help"] {
        if let Some(t) = tools::find(name) {
            assert!(
                !t.mutating,
                "{name} only reads and must not be marked mutating"
            );
        }
    }
}

// -------------------------------------------------- dashboard API surface ---

/// Every dashboard route must be exercised by a test.
///
/// MECHANICAL BY DESIGN. The cross-board actuation bug reached hardware through
/// `/api/power`, a route with no test of its own, and the dashboard is the path
/// a human takes -- it is not covered by the MCP tests just because both end up
/// in mcpd. A route added tomorrow fails this until someone covers it.
#[test]
fn every_dashboard_route_is_covered_by_a_test() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/dash.rs"))
        .expect("dash.rs");

    // Routes as the router declares them.
    let mut routes: Vec<String> = Vec::new();
    for line in src.lines() {
        let Some(i) = line.find(".route(\"") else {
            continue;
        };
        let rest = &line[i + 8..];
        let Some(end) = rest.find('"') else { continue };
        routes.push(rest[..end].to_string());
    }
    assert!(routes.len() >= 4, "did not parse the router: {routes:?}");

    // Every test file that drives the dashboard.
    let mut covered = String::new();
    for f in ["dash/main.rs", "surface/main.rs", "e2e/main.rs"] {
        let p = format!("{}/tests/{f}", env!("CARGO_MANIFEST_DIR"));
        if let Ok(t) = std::fs::read_to_string(p) {
            covered.push_str(&t);
        }
    }

    let missing: Vec<&String> = routes
        .iter()
        .filter(|r| {
            // Match on the stable prefix, since tests substitute real selectors
            // for the `:param` segments.
            let stem = r.split(':').next().unwrap_or(r).trim_end_matches('/');
            stem.len() > 1 && !covered.contains(stem)
        })
        .collect();
    assert!(
        missing.is_empty(),
        "these dashboard routes have no test driving them: {missing:?}"
    );
}

/// The dashboard's own controls must exist in the page it serves.
///
/// The UI is where a human presses things, and a control that vanished from the
/// page is invisible to every server-side test. This pins the controls that
/// actuate hardware, plus the power indicator, to the page itself.
#[test]
fn the_dashboard_page_carries_its_hardware_controls() {
    let page = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/dashboard.html"))
        .expect("dashboard.html");

    for control in ["/api/power/", "/api/boot_mode/", "/ws/console/"] {
        assert!(
            page.contains(control),
            "the page no longer reaches {control} -- a control disappeared"
        );
    }
    for action in ["on", "off", "cycle"] {
        assert!(
            page.contains(&format!("\"{action}\"")) || page.contains(&format!("'{action}'")),
            "the page offers no {action} control"
        );
    }
    // The power lamp: a human's at-a-glance answer to "is this board on?".
    assert!(
        page.contains("pwr"),
        "the power indicator is gone from the page"
    );
    assert!(
        page.contains("power n/a") || page.contains("cannot sense"),
        "a controller that cannot measure power must still say so in the UI"
    );
}

/// A multi-console board is a target without anyone writing config.
///
/// S7 FROM THE SWEEP: `list_targets` returned empty on a rig with three
/// multi-console boards, so `target_context` and `target_mark` -- the tools for
/// "show me every console of this board around this moment" -- were dead exactly
/// where they matter (IQ10 AP+SAIL, NordAU AP+safety-monitor+4 more). The config
/// was never written and was never going to be. The consoles of one board
/// already share a USB hub, which is the same signal that binds a controller to
/// the right board, so a board is a target by default.
#[test]
fn a_boards_consoles_form_a_target_with_no_config() {
    use conminer_core::config::topology_group;

    // Four consoles of one board, one console of another.
    let board_a = [
        "pci-0000:00:14.0-usb-0:3.1.1:1.0",
        "pci-0000:00:14.0-usb-0:3.1.2:1.0",
        "pci-0000:00:14.0-usb-0:3.1.3:1.0",
        "pci-0000:00:14.0-usb-0:3.1.4:1.0",
    ];
    let board_b = "pci-0000:00:14.0-usb-0:3.2.1:1.0";

    let a: Vec<Option<String>> = board_a.iter().map(|p| topology_group(Some(p))).collect();
    let first = a[0].clone().expect("a topology group");
    assert!(
        a.iter().all(|g| *g == a[0]),
        "every console of one board must land in one group: {a:?}"
    );
    assert_ne!(
        topology_group(Some(board_b)),
        Some(first),
        "a different board must be a different target"
    );
}

/// `decode` must resolve addresses out of the box.
///
/// S6 FROM THE SWEEP: every decode returned `regions_known: 0`. The errno/ESR/
/// GIC decoding worked, but address-to-region resolution -- the part that
/// matters most for crash triage -- was inert because the memory map is
/// per-device config and nobody had written it. A feature that needs
/// configuration nobody writes is a feature that does not exist.
#[test]
fn address_decoding_works_without_per_device_config() {
    let cfg = conminer_core::config::Config::default();
    let map = cfg.memory_map_for(&["a-device-nobody-configured"]);
    assert!(
        !map.is_empty(),
        "a device with no config of its own must still inherit the rig map"
    );

    // The blocks the boards actually complain about must resolve.
    for (name, probe) in [
        ("dwc3-usb", 0x0a60_0000u64),
        ("gmu", 0x03d6_a000),
        ("mdss-dpu", 0x0ae0_1000),
    ] {
        let hit = map
            .iter()
            .find(|r| probe >= r.base && probe < r.base + r.size);
        let hit = hit.unwrap_or_else(|| panic!("{probe:#x} resolves to nothing; {name} missing"));
        assert_eq!(hit.name, name, "{probe:#x} resolved to the wrong region");
    }

    // Regions must be sane: no zero-size entry, no overlap that would make a
    // resolution ambiguous.
    for r in &map {
        assert!(r.size > 0, "{} has zero size", r.name);
    }
    for (i, a) in map.iter().enumerate() {
        for b in map.iter().skip(i + 1) {
            let overlap = a.base < b.base + b.size && b.base < a.base + a.size;
            assert!(
                !overlap,
                "{} and {} overlap; resolution is ambiguous",
                a.name, b.name
            );
        }
    }
}

/// A diff must not report "I did not look" as "this stopped happening".
///
/// S4 FROM THE SWEEP: diffing an epoch that contained the firmware stages
/// against one that began at the kernel reported `gone_from_b: 497` -- every
/// firmware line presented as a regression, when B simply never covered that
/// stage. Epoch boundaries differ by how they were opened (power vs reset vs
/// session vs mark), so this is not an edge case. Phantom regressions are
/// exactly the shape of real ones, which makes the tool actively misleading.
#[test]
fn a_diff_says_which_stages_it_could_not_compare() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../conminer-mcp/src/report.rs"
    ))
    .expect("report.rs");

    assert!(
        src.contains("COMPARE ONLY THE STAGES BOTH EPOCHS ACTUALLY COVERED"),
        "the restriction must exist and carry its reasoning"
    );
    for field in [
        "stages_compared",
        "stages_only_in_a",
        "stages_only_in_b",
        "scope_note",
    ] {
        assert!(
            src.contains(field),
            "a restricted comparison must report `{field}`, or a clean diff reads as agreement"
        );
    }
    // A template with no stage must be KEPT: dropping it would hide real changes
    // in order to silence the phantoms, which trades one wrong answer for another.
    assert!(
        src.contains("cannot be placed, so it is kept"),
        "unattributed templates must not be silently dropped"
    );
}
