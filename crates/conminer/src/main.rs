//! `conminer` — one binary, several entrypoints (§9).
//!
//!   conminer discoveryd | minerd | mcpd | ser2net-supervisor   services
//!   conminer ingest | templates | records | context | search    post-hoc work
//!   conminer check-config | profile test | profiles             operator tools
//!
//! Everything the CLI can do, the MCP surface can do too — the CLI exists so a
//! human can reproduce, by hand, exactly what an agent saw.

mod app;
mod bughopper;
mod output;
mod query;
mod service;
mod tac;

use anyhow::Result;
use app::App;
use clap::{Parser, Subcommand};
use conminer_core::store::TemplateOrder;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "conminer",
    version,
    about = "Serial console template miner for LLM agents",
    long_about = None
)]
struct Cli {
    /// Path to conminer.toml. Defaults to $CONMINER_CONFIG, then built-in defaults.
    #[arg(long, global = true, env = "CONMINER_CONFIG")]
    config: Option<PathBuf>,

    /// Data directory holding registry.db and the per-device stores.
    #[arg(long, global = true, env = "CONMINER_DATA")]
    data: Option<PathBuf>,

    /// Machine-readable output. Every command supports it.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    // ---- services (§3) ----
    /// Watch /dev/serial/by-id, maintain the registry, regenerate ser2net.yaml.
    Discoveryd,
    /// Find peer nodes and keep the fleet's inventory fresh (§P1).
    Peerd,
    /// Live capture + framing + mining for every discovered device.
    Minerd,
    /// MCP server: streamable HTTP, and stdio for `docker exec`.
    Mcpd {
        /// Serve MCP over stdio instead of HTTP.
        #[arg(long)]
        stdio: bool,
    },
    /// Human dashboard: live console list, connection strings, watch and transmit.
    Dashd,
    /// Run ser2net against the generated config, reloading on registry change.
    Ser2netSupervisor,
    /// Power / reset / EDL for a Bughopper-class FTDI CBUS controller.
    ///
    /// Invoked as a hook by the `bughopper` controller profile. Native rather
    /// than a script: the whole operation is one USB control transfer, and
    /// doing it in-process means the same process that takes the interface from
    /// ftdi_sio hands it back, so the console cannot be left missing.
    BughopperPower {
        /// on | off | cycle | reset | id | state | mode
        action: String,
        /// Boot mode for `mode` (only EDL is wired on CBUS).
        #[arg(default_value = "")]
        arg: String,
        /// Seconds to hold the power button for an off (PMIC long-press).
        #[arg(long, default_value_t = 6.0)]
        settle: f64,
        /// Target a specific controller when several are attached.
        #[arg(long, default_value = "")]
        serial: String,
        /// Console by-id path; its embedded USB serial identifies the controller.
        #[arg(long, default_value = "")]
        device: String,
        /// Do not hand the interface back to ftdi_sio afterwards.
        ///
        /// Rebinding resets the FTDI, which returns CBUS to its EEPROM defaults
        /// and can pulse the very lines we just drove -- switching a board that
        /// was told to power OFF straight back on. Diagnostic escape hatch, and
        /// the mechanism `off` uses to stay off.
        #[arg(long)]
        no_rebind: bool,
    },
    /// Power / reset / EDL / UEFI / fastboot for an FTDI TAC ("Alpaca") board.
    ///
    /// Invoked as a hook by the `tac` controller profile. The TAC's GPIO
    /// channels are the same FTDI that carries the board's consoles, so this is
    /// native for the Bughopper's reason and one more: the pins are LEVELS, and
    /// a script killed mid-sequence would leave a strap latched.
    TacPower {
        /// on | off | cycle | reset | id | power-state | mode
        action: String,
        /// Boot mode for `mode`: EDL | SAIL_EDL | UEFI | FASTBOOT | clear.
        #[arg(default_value = "")]
        arg: String,
        /// Seconds to hold the board off before bringing it back (the rail
        /// collapse the vendor's own sequences wait 1.5s for).
        #[arg(long, default_value_t = 1.5)]
        settle: f64,
        /// Target a specific TAC when several are attached.
        #[arg(long, default_value = "")]
        serial: String,
        /// Console by-id path; its embedded USB serial identifies the TAC.
        #[arg(long, default_value = "")]
        device: String,
        /// Resolve the board and print the pin sequence, driving nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Compose healthcheck probe.
    Healthcheck {
        #[arg(long)]
        service: String,
    },

    // ---- post-hoc (§4) ----
    /// Mine a log file. 10 KB to 800 MB, gzip auto-detected.
    Ingest {
        path: PathBuf,
        /// Device selector; defaults to a synthetic device keyed on the file path.
        #[arg(long)]
        device: Option<String>,
        /// Pin a framer profile instead of auto-detecting.
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        label: Option<String>,
    },

    // ---- queries (§8) ----
    /// The table of contents: deduplicated templates with counts.
    Templates {
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        session: Option<i64>,
        #[arg(long)]
        boot: Option<i64>,
        #[arg(long)]
        stage: Option<String>,
        #[arg(long)]
        min_count: Option<i64>,
        /// Only templates first seen in the scoped session.
        #[arg(long)]
        new_only: bool,
        /// count | first-seen | last-seen | severity
        #[arg(long, default_value = "count")]
        order: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Verbatim raw records for a template.
    Records {
        template: i64,
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        session: Option<i64>,
        #[arg(long, default_value_t = 3)]
        limit: usize,
    },
    /// ±N verbatim raw lines around a line.
    Context {
        line: i64,
        #[arg(long)]
        device: Option<String>,
        #[arg(long, default_value_t = 10)]
        before: usize,
        #[arg(long, default_value_t = 10)]
        after: usize,
    },
    /// Regex over the raw store (uart-mcp `query_serial_logs` parity).
    Search {
        pattern: String,
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        session: Option<i64>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Tail of a device's stream (uart-mcp `get_recent_logs` parity).
    Recent {
        #[arg(long)]
        device: Option<String>,
        #[arg(long, default_value_t = 50)]
        lines: usize,
    },
    /// Line/record/template counts, compression ratio, index size.
    Stats {
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        session: Option<i64>,
    },
    /// Session history, including file ingests.
    Sessions {
        #[arg(long)]
        device: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Boot-epoch history with outcomes and fingerprints.
    Boots {
        #[arg(long)]
        device: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Stage timeline with banner line refs.
    Stages {
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        session: Option<i64>,
        #[arg(long)]
        boot: Option<i64>,
    },
    /// Devices, endpoints, active profile/stage, observed identity.
    Devices,
    /// Run the acceptance gauntlet against a real board (§K1).
    ///
    /// Exit codes: 0 pass, 1 fail, 2 pass_with_skips, 3 cleanup unverified.
    Selftest {
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        device: Option<String>,
        /// capture,actuation,edl,mining,provenance,honesty (default: all)
        #[arg(long, value_delimiter = ',')]
        suites: Vec<String>,
        /// Take leases held by others.
        #[arg(long)]
        steal: bool,
        /// Leave the board powered at the end. Off by default, always.
        #[arg(long)]
        keep_on: bool,
    },
    /// Fill in firmware versions for epochs captured before the banner
    /// extractors existed, without re-minting a single template (§K5b).
    BackfillVersions {
        #[arg(long)]
        device: Option<String>,
    },
    /// Regenerate every template from raw — how a threshold change is applied
    /// retroactively (§6).
    RebuildTemplates {
        #[arg(long)]
        device: Option<String>,
        /// Override mine.similarity for the rebuild.
        #[arg(long)]
        similarity: Option<f64>,
    },

    // ---- operator tools (§14.5, §14.11) ----
    /// Validate conminer.toml and report every problem, not just the first.
    CheckConfig,
    /// List available framer profiles.
    Profiles,
    /// Develop a profile against your own logs without touching Rust (§14.11).
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
}

#[derive(Subcommand, Debug)]
enum ProfileCmd {
    /// Frame and mine a corpus file with one profile and show what it produced.
    Test {
        profile: String,
        corpus: PathBuf,
        /// Show every record, not just a summary.
        #[arg(long)]
        verbose: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    match &cli.cmd {
        // Services own their own runtime setup.
        Cmd::Discoveryd => return service::discoveryd(&cli.config, &cli.data),
        Cmd::Peerd => return service::peerd(&cli.config, &cli.data),
        Cmd::Minerd => return service::minerd(&cli.config, &cli.data),
        Cmd::Mcpd { stdio } => return service::mcpd(&cli.config, &cli.data, *stdio),
        Cmd::Dashd => return service::dashd(&cli.config, &cli.data),
        Cmd::Ser2netSupervisor => return service::ser2net_supervisor(&cli.config, &cli.data),
        Cmd::BughopperPower {
            action,
            arg,
            settle,
            serial,
            device,
            no_rebind,
        } => {
            return bughopper::run(action, arg, *settle, serial, device, *no_rebind);
        }
        Cmd::TacPower {
            action,
            arg,
            settle,
            serial,
            device,
            dry_run,
        } => {
            return tac::run(action, arg, *settle, serial, device, *dry_run);
        }
        Cmd::Healthcheck { service } => {
            return service::healthcheck(&cli.config, &cli.data, service)
        }
        _ => {}
    }

    let app = App::load(cli.config.as_deref(), cli.data.as_deref())?;
    let out = output::Writer::new(cli.json);

    match cli.cmd {
        Cmd::Ingest {
            path,
            device,
            profile,
            label,
        } => query::ingest(
            &app,
            &out,
            &path,
            device.as_deref(),
            profile.as_deref(),
            label,
        ),
        Cmd::Templates {
            device,
            session,
            boot,
            stage,
            min_count,
            new_only,
            order,
            limit,
        } => query::templates(
            &app,
            &out,
            device.as_deref(),
            conminer_core::store::TemplateQuery {
                session_id: session,
                boot_id: boot,
                stage,
                min_count,
                min_severity: None,
                new_only,
                // The CLI is the raw view by design: it is what you reach for
                // when you want to see everything, including what an agent has
                // annotated away.
                not_in_boot: None,
                only_verdicts: Vec::new(),
                hide_verdicts: Vec::new(),
                order: parse_order(&order)?,
                limit,
                offset: 0,
            },
        ),
        Cmd::Records {
            template,
            device,
            session,
            limit,
        } => query::records(&app, &out, device.as_deref(), template, session, limit),
        Cmd::Context {
            line,
            device,
            before,
            after,
        } => query::context(&app, &out, device.as_deref(), line, before, after),
        Cmd::Search {
            pattern,
            device,
            session,
            limit,
        } => query::search(&app, &out, device.as_deref(), &pattern, session, limit),
        Cmd::Recent { device, lines } => query::recent(&app, &out, device.as_deref(), lines),
        Cmd::Stats { device, session } => query::stats(&app, &out, device.as_deref(), session),
        Cmd::Sessions { device, limit } => query::sessions(&app, &out, device.as_deref(), limit),
        Cmd::Boots { device, limit } => query::boots(&app, &out, device.as_deref(), limit),
        Cmd::Stages {
            device,
            session,
            boot,
        } => query::stages(&app, &out, device.as_deref(), session, boot),
        Cmd::Devices => query::devices(&app, &out),
        Cmd::Selftest {
            target,
            device,
            suites,
            steal,
            keep_on,
        } => query::selftest(&app, &out, target, device, suites, steal, keep_on),
        Cmd::BackfillVersions { device } => query::backfill_versions(&app, &out, device.as_deref()),
        Cmd::RebuildTemplates { device, similarity } => {
            query::rebuild_templates(&app, &out, device.as_deref(), similarity)
        }
        Cmd::CheckConfig => query::check_config(&app, &out, cli.config.as_deref()),
        Cmd::Profiles => query::profiles(&app, &out),
        Cmd::Profile {
            cmd:
                ProfileCmd::Test {
                    profile,
                    corpus,
                    verbose,
                },
        } => query::profile_test(&app, &out, &profile, &corpus, verbose),
        _ => unreachable!("services returned above"),
    }
}

fn parse_order(s: &str) -> Result<TemplateOrder> {
    Ok(match s {
        "count" => TemplateOrder::Count,
        "first-seen" | "first_seen" => TemplateOrder::FirstSeen,
        "last-seen" | "last_seen" => TemplateOrder::LastSeen,
        "severity" => TemplateOrder::Severity,
        other => anyhow::bail!("unknown order {other:?}; use count|first-seen|last-seen|severity"),
    })
}

/// Structured JSON logs to stderr (§14.2), honouring the rule that conminer's
/// own logs are excluded from mining by default — they never touch a device
/// store, only stderr.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .expect("valid filter");
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    if std::env::var("CONMINER_LOG_FORMAT").as_deref() == Ok("text") {
        let _ = builder.try_init();
    } else {
        let _ = builder.json().try_init();
    }
}
