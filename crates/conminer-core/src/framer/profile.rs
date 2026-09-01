//! Declarative framer profiles (§5, Appendix A).
//!
//! A profile is the A.0 meta-grammar filled in for one firmware:
//!
//! ```text
//! [BANNER]* → normal traffic → TRIGGER → CONTEXT-DUMP* → BACKTRACE*
//!           → TERMINATOR? → (RESET-MARKER | silence)
//! ```
//!
//! expressed as (banner set, severity map, trigger set, context-dump matcher,
//! backtrace matcher, terminator set, reset markers) — which is exactly the TOML
//! schema below. Users drop a file in `profiles.d/` and get a new firmware
//! supported without recompiling; the native tier (a Rust `Framer` impl) exists
//! for the stateful cases the tuple cannot express.
//!
//! If a future firmware cannot be expressed in the tuple, the meta-grammar gets
//! amended — with a test — not the firmware special-cased.

use super::generic;
use super::{FramedRecord, Framer, FramerEvent, FramerInput};
use crate::drain::TokenizerRules;
use crate::error::{ErrorCode, Result, ToolError};
use crate::store::{RecordKind, Severity};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------- schema -----

fn t() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeverityRuleDef {
    pub pattern: String,
    pub level: Severity,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerDef {
    pub pattern: String,
    #[serde(default = "default_trigger_severity")]
    pub severity: Severity,
    /// `crash` opens a fatal record; `line` marks a record-worthy but non-fatal
    /// event (a WARNING, a hung-task report) that still groups its dump.
    #[serde(default = "default_trigger_kind")]
    pub kind: RecordKind,
    /// Pull preceding lines matching the profile's `lookback` patterns into the
    /// record. Zephyr prints its Cortex-M fault decode *before* the FATAL banner,
    /// and a Python traceback puts its frames before the exception line — an
    /// inverted record shape the framer must retro-attach.
    #[serde(default)]
    pub lookback: bool,
}

fn default_trigger_severity() -> Severity {
    Severity::Crit
}
fn default_trigger_kind() -> RecordKind {
    RecordKind::Crash
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredLineDef {
    /// Regex with named captures. `{name}` in `rewrite` is substituted by the
    /// capture of that name.
    pub pattern: String,
    /// The mining key to use instead, e.g. `"{prefix} - {label} - {value}"`.
    pub rewrite: String,
}

#[derive(Debug, Clone)]
pub struct StructuredLine {
    pub re: Regex,
    pub rewrite: String,
}

impl StructuredLine {
    /// Rewrite a line into its mining key, or leave it alone.
    pub fn apply(&self, line: &str) -> Option<String> {
        let c = self.re.captures(line)?;
        let mut out = self.rewrite.clone();
        for name in self.re.capture_names().flatten() {
            let val = c.name(name).map(|m| m.as_str()).unwrap_or("");
            out = out.replace(&format!("{{{name}}}"), val.trim());
        }
        Some(out)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VersionBannerDef {
    /// Regex with a named `version` capture; `build_date`, `builder`, `build`
    /// and `model` are lifted into `detail` when present.
    pub pattern: String,
    /// Which component this identifies: bl2, bl31, optee, uefi, xbl, kernel,
    /// machine...
    pub component: String,
}

#[derive(Debug, Clone)]
pub struct VersionBanner {
    pub re: Regex,
    pub component: String,
}

impl VersionBanner {
    /// Pull the version out of a line, with whatever else the banner carried.
    pub fn extract(&self, line: &str) -> Option<(String, serde_json::Value)> {
        let c = self.re.captures(line)?;
        // A truncated or noise-mangled banner must not be stored as a version:
        // only a full match counts, and only with the capture that names it.
        let version = c.name("version")?.as_str().trim().to_string();
        if version.is_empty() {
            return None;
        }
        let mut detail = serde_json::Map::new();
        for key in ["build_date", "builder", "build", "model", "name"] {
            if let Some(m) = c.name(key) {
                detail.insert(key.to_string(), serde_json::json!(m.as_str().trim()));
            }
        }
        Some((version, serde_json::Value::Object(detail)))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BannerDef {
    pub pattern: String,
    /// Stage this banner enters. Defaults to the profile's own stage.
    pub stage: Option<String>,
    /// Boot-order rank of *this* stage, defaulting to the profile's.
    ///
    /// Rank has to be per banner, not per profile: U-Boot SPL legitimately runs
    /// *before* TF-A BL31, which then hands back to U-Boot proper. A single
    /// per-profile rank would make that ordinary chain look like a reboot.
    pub rank: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromptDef {
    pub pattern: String,
    pub kind: PromptKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    Shell,
    Bootloader,
    RtosShell,
    Monitor,
    CredentialGate,
    Ignore,
}

impl PromptKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptKind::Shell => "shell",
            PromptKind::Bootloader => "bootloader",
            PromptKind::RtosShell => "rtos_shell",
            PromptKind::Monitor => "monitor",
            PromptKind::CredentialGate => "credential_gate",
            PromptKind::Ignore => "ignore",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "shell" => PromptKind::Shell,
            "bootloader" => PromptKind::Bootloader,
            "rtos_shell" => PromptKind::RtosShell,
            "monitor" => PromptKind::Monitor,
            "credential_gate" => PromptKind::CredentialGate,
            "ignore" => PromptKind::Ignore,
            _ => return None,
        })
    }

    /// A credential gate is never a shell prompt: the board is up but not
    /// commandable, and `prompt:true` must not fire on it (§8.5).
    pub fn is_commandable(self) -> bool {
        matches!(
            self,
            PromptKind::Shell
                | PromptKind::Bootloader
                | PromptKind::RtosShell
                | PromptKind::Monitor
        )
    }
}

/// The on-disk profile. Every field is optional except `name`, so a minimal
/// project profile is a handful of lines.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDef {
    pub name: String,
    /// Stage this profile serves (`kernel`, `uboot`, `bl31`…).
    pub stage: Option<String>,
    /// Boot ordering, used for reset detection: a banner at or before the
    /// earliest stage already seen means the target restarted (§A.0).
    #[serde(default)]
    pub stage_rank: i32,
    /// This dialect interleaves *inside* another rather than replacing it —
    /// OP-TEE core messages inside Linux output, `(XEN)` over dom0.
    #[serde(default)]
    pub overlay: bool,
    /// Patterns that claim a single line for an overlay dialect. A banner is not
    /// enough: OP-TEE's abort lines carry `E/TC:`, not its version banner, and
    /// attributing them to the kernel would blame the wrong component.
    #[serde(default)]
    pub overlay_match: Vec<String>,
    #[serde(default)]
    pub banners: Vec<BannerDef>,
    /// Lines that state WHAT IS RUNNING (§F2).
    ///
    /// Distinct from `banners`, which answer "which stage is this?". A version
    /// banner answers "which build?" -- BL31's fingerprint, OP-TEE's commit, the
    /// UEFI string, the kernel version and its `#build`. That data was in every
    /// store for four rounds while `provenance.running` returned `{}`, because
    /// nothing lifted it out.
    #[serde(default)]
    pub version_banners: Vec<VersionBannerDef>,
    /// Lines whose grammar the generic templatizer destroys (§F3).
    ///
    /// Qualcomm's XBL timing log is the case that forced this: `B - 12345 -
    /// sbl1_ddr_init` and `B - 67890 - sbl1_hw_init` differ in BOTH the number
    /// and the label, so Drain generalises both and every timing line in the
    /// boot collapses into one `D - <*> - <*>` template -- 6,755 occurrences on
    /// the ADP, and per-label timing became unqueryable. Clustering keys on the
    /// LEADING tokens, so putting the label in front and the number last makes
    /// each label its own template with the number in a readable slot.
    #[serde(default)]
    pub structured_lines: Vec<StructuredLineDef>,
    #[serde(default)]
    pub reset_markers: Vec<String>,
    #[serde(default)]
    pub severity: Vec<SeverityRuleDef>,
    #[serde(default)]
    pub triggers: Vec<TriggerDef>,
    #[serde(default)]
    pub context: Vec<String>,
    #[serde(default)]
    pub backtrace_headers: Vec<String>,
    #[serde(default)]
    pub backtrace_frames: Vec<String>,
    #[serde(default)]
    pub terminators: Vec<String>,
    /// Lines that continue an open record regardless of state.
    #[serde(default)]
    pub continuations: Vec<String>,
    /// Lines eligible for retro-attachment to a following trigger.
    #[serde(default)]
    pub lookback: Vec<String>,
    /// Non-destructive field extraction: `field = regex`, taking the first
    /// capture group (or the named group matching the field).
    #[serde(default)]
    pub extract: BTreeMap<String, String>,
    /// Spans removed from the **mining key only**. The raw line is untouched and
    /// the removed span is recoverable from `extract`.
    #[serde(default)]
    pub mine_strip: Vec<String>,
    /// A marker that, appearing MID-LINE, means a new message interleaved into
    /// whatever was being printed. Mining restarts there.
    ///
    /// Consoles interleave: systemd writes a status line with no newline and a
    /// kernel printk lands in the middle of it. Measured on a real boot, that
    /// produced templates like
    ///     "Starting DNS forwarder and DHCP server... [ 16.470867] read descriptors"
    ///     "ventuno-q-535812139 login: <*> <*> read descriptors"
    /// -- one kernel message wearing eight different prefixes, so it never
    /// clustered with itself. The prefixes are real bytes the board sent and stay
    /// in raw; this only trims the DERIVED mining key.
    #[serde(default)]
    pub mine_resync: Option<String>,
    #[serde(default)]
    pub prompts: Vec<PromptDef>,
    #[serde(default)]
    pub tokenizer: TokenizerRules,
    /// Overrides `framer.record_timeout_s`. UEFI and TF-A raise it because they
    /// dead-loop after a panic instead of printing a terminator.
    pub record_timeout_s: Option<u64>,
    /// Strip ANSI escape sequences from the display/mining view (raw untouched).
    #[serde(default)]
    pub ansi_strip: bool,
    /// Fall back to the §A.10 generic classes for anything not matched above.
    #[serde(default = "t")]
    pub use_generic: bool,
}

// -------------------------------------------------------------- compiled -----

#[derive(Debug)]
pub struct Banner {
    pub re: Regex,
    pub stage: String,
    pub rank: i32,
}

#[derive(Debug)]
pub struct Trigger {
    pub re: Regex,
    pub severity: Severity,
    pub kind: RecordKind,
    pub lookback: bool,
}

#[derive(Debug)]
pub struct SeverityRule {
    pub re: Regex,
    pub level: Severity,
}

#[derive(Debug)]
pub struct PromptPattern {
    pub re: Regex,
    pub raw: String,
    pub kind: PromptKind,
}

/// A compiled, ready-to-run profile.
#[derive(Debug)]
pub struct Profile {
    pub name: String,
    pub stage: String,
    pub stage_rank: i32,
    pub overlay: bool,
    pub banners: Vec<Banner>,
    pub version_banners: Vec<VersionBanner>,
    pub structured_lines: Vec<StructuredLine>,
    pub overlay_match: Vec<Regex>,
    pub reset_markers: Vec<Regex>,
    pub severity: Vec<SeverityRule>,
    pub triggers: Vec<Trigger>,
    pub context: Vec<Regex>,
    pub backtrace_headers: Vec<Regex>,
    pub backtrace_frames: Vec<Regex>,
    pub terminators: Vec<Regex>,
    pub continuations: Vec<Regex>,
    pub lookback: Vec<Regex>,
    pub extract: Vec<(String, Regex)>,
    pub mine_strip: Vec<Regex>,
    pub mine_resync: Option<Regex>,
    pub prompts: Vec<PromptPattern>,
    pub tokenizer: TokenizerRules,
    pub record_timeout_s: Option<u64>,
    pub ansi_strip: bool,
    pub use_generic: bool,
}

static ANSI: once_cell::sync::Lazy<Regex> = once_cell::sync::Lazy::new(|| {
    Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[@-Z\\-_]").expect("ansi regex")
});

/// Remove ANSI escape sequences. Used for the display and mining views only.
pub fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    ANSI.replace_all(s, "")
}

fn compile_all(pats: &[String], what: &str, profile: &str) -> Result<Vec<Regex>> {
    pats.iter()
        .map(|p| {
            Regex::new(p).map_err(|e| {
                ToolError::new(
                    ErrorCode::InvalidConfig,
                    format!("profile {profile:?} {what} regex {p:?}: {e}"),
                )
            })
        })
        .collect()
}

impl Profile {
    pub fn compile(def: ProfileDef) -> Result<Self> {
        let name = def.name.clone();
        let stage = def.stage.clone().unwrap_or_else(|| name.clone());
        let one = |p: &str, what: &str| -> Result<Regex> {
            Regex::new(p).map_err(|e| {
                ToolError::new(
                    ErrorCode::InvalidConfig,
                    format!("profile {name:?} {what} regex {p:?}: {e}"),
                )
            })
        };

        Ok(Profile {
            structured_lines: def
                .structured_lines
                .iter()
                .map(|v| {
                    Ok(StructuredLine {
                        re: one(&v.pattern, "structured_line")?,
                        rewrite: v.rewrite.clone(),
                    })
                })
                .collect::<Result<_>>()?,
            version_banners: def
                .version_banners
                .iter()
                .map(|v| {
                    Ok(VersionBanner {
                        re: one(&v.pattern, "version_banner")?,
                        component: v.component.clone(),
                    })
                })
                .collect::<Result<_>>()?,
            banners: def
                .banners
                .iter()
                .map(|b| {
                    Ok(Banner {
                        re: one(&b.pattern, "banner")?,
                        stage: b.stage.clone().unwrap_or_else(|| stage.clone()),
                        rank: b.rank.unwrap_or(def.stage_rank),
                    })
                })
                .collect::<Result<_>>()?,
            overlay_match: compile_all(&def.overlay_match, "overlay_match", &name)?,
            reset_markers: compile_all(&def.reset_markers, "reset_marker", &name)?,
            severity: def
                .severity
                .iter()
                .map(|s| {
                    Ok(SeverityRule {
                        re: one(&s.pattern, "severity")?,
                        level: s.level,
                    })
                })
                .collect::<Result<_>>()?,
            triggers: def
                .triggers
                .iter()
                .map(|t| {
                    Ok(Trigger {
                        re: one(&t.pattern, "trigger")?,
                        severity: t.severity,
                        kind: t.kind,
                        lookback: t.lookback,
                    })
                })
                .collect::<Result<_>>()?,
            context: compile_all(&def.context, "context", &name)?,
            backtrace_headers: compile_all(&def.backtrace_headers, "backtrace_header", &name)?,
            backtrace_frames: compile_all(&def.backtrace_frames, "backtrace_frame", &name)?,
            terminators: compile_all(&def.terminators, "terminator", &name)?,
            continuations: compile_all(&def.continuations, "continuation", &name)?,
            lookback: compile_all(&def.lookback, "lookback", &name)?,
            extract: def
                .extract
                .iter()
                .map(|(k, v)| Ok((k.clone(), one(v, "extract")?)))
                .collect::<Result<_>>()?,
            mine_strip: compile_all(&def.mine_strip, "mine_strip", &name)?,
            mine_resync: match &def.mine_resync {
                Some(p) => compile_all(std::slice::from_ref(p), "mine_resync", &name)?
                    .into_iter()
                    .next(),
                None => None,
            },
            prompts: def
                .prompts
                .iter()
                .map(|p| {
                    Ok(PromptPattern {
                        re: one(&p.pattern, "prompt")?,
                        raw: p.pattern.clone(),
                        kind: p.kind,
                    })
                })
                .collect::<Result<_>>()?,
            name,
            stage,
            stage_rank: def.stage_rank,
            overlay: def.overlay,
            tokenizer: def.tokenizer,
            record_timeout_s: def.record_timeout_s,
            ansi_strip: def.ansi_strip,
            use_generic: def.use_generic,
        })
    }

    pub fn from_toml(s: &str) -> Result<Self> {
        let def: ProfileDef = toml::from_str(s)
            .map_err(|e| ToolError::new(ErrorCode::InvalidConfig, format!("profile parse: {e}")))?;
        Self::compile(def)
    }

    /// Which stage (if any) this line's banner enters.
    pub fn banner_stage(&self, text: &str) -> Option<&str> {
        self.banners
            .iter()
            .find(|b| b.re.is_match(text))
            .map(|b| b.stage.as_str())
    }

    /// Does this overlay dialect claim the line? Falls back to banners for a
    /// profile that declares no explicit signature.
    pub fn claims(&self, text: &str) -> bool {
        if !self.overlay_match.is_empty() {
            return self.overlay_match.iter().any(|r| r.is_match(text));
        }
        self.banner_stage(text).is_some()
    }

    pub fn is_reset_marker(&self, text: &str) -> bool {
        self.reset_markers.iter().any(|r| r.is_match(text))
            || (self.use_generic && generic::is_reset_marker(text))
    }

    pub fn trigger(&self, text: &str) -> Option<&Trigger> {
        self.triggers.iter().find(|t| t.re.is_match(text))
    }

    pub fn severity_of(&self, text: &str) -> Severity {
        for r in &self.severity {
            if r.re.is_match(text) {
                return r.level;
            }
        }
        if self.use_generic {
            generic::generic_severity(text)
        } else {
            Severity::Unknown
        }
    }

    pub fn is_context(&self, text: &str) -> bool {
        self.context.iter().any(|r| r.is_match(text))
            || (self.use_generic && generic::is_register_line(text))
    }

    pub fn is_backtrace_header(&self, text: &str) -> bool {
        self.backtrace_headers.iter().any(|r| r.is_match(text))
            || (self.use_generic && generic::is_backtrace_header(text))
    }

    pub fn is_backtrace_frame(&self, text: &str) -> bool {
        self.backtrace_frames.iter().any(|r| r.is_match(text))
            || (self.use_generic && generic::is_backtrace_frame(text))
    }

    pub fn is_terminator(&self, text: &str) -> bool {
        self.terminators.iter().any(|r| r.is_match(text))
    }

    pub fn is_continuation(&self, text: &str) -> bool {
        self.continuations.iter().any(|r| r.is_match(text))
    }

    pub fn is_lookback(&self, text: &str) -> bool {
        self.lookback.iter().any(|r| r.is_match(text))
    }

    pub fn prompt_match<'a>(&'a self, text: &str) -> Option<&'a PromptPattern> {
        self.prompts.iter().find(|p| p.re.is_match(text))
    }

    /// Non-destructive extraction. The raw line is never modified; these values
    /// are stored beside it.
    pub fn extract_fields(&self, text: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut out = serde_json::Map::new();
        for (field, re) in &self.extract {
            if let Some(c) = re.captures(text) {
                let v = c
                    .name(field)
                    .or_else(|| c.get(1))
                    .or_else(|| c.get(0))
                    .map(|m| m.as_str().to_string());
                if let Some(v) = v {
                    out.insert(field.clone(), serde_json::Value::String(v));
                }
            }
        }
        out
    }

    /// The derived view Drain clusters on: display text with already-extracted
    /// spans (printk timestamps, log prefixes) removed. Raw stays raw.
    /// Rewrite a structured line into its mining key, if a rule claims it.
    fn structured_key(&self, s: &str) -> Option<String> {
        self.structured_lines.iter().find_map(|r| r.apply(s))
    }

    pub fn mine_key(&self, text: &str) -> String {
        let base = if self.ansi_strip {
            strip_ansi(text).into_owned()
        } else {
            text.to_string()
        };
        // A leading UTF-8 BOM is a file artefact, not part of the message; it
        // stays in the stored bytes and is dropped only from this derived view.
        let mut s = base.trim_start_matches('\u{feff}').to_string();
        // Resync FIRST: a marker mid-line means everything before it belongs to
        // a different message, and only after trimming that can the ordinary
        // leading-prefix strips match.
        if let Some(re) = &self.mine_resync {
            if let Some(m) = re.find(&s) {
                if m.start() > 0 {
                    s = s[m.start()..].to_string();
                }
            }
        }
        for re in &self.mine_strip {
            s = re.replace_all(&s, "").into_owned();
        }
        let s = s.trim();
        // Last, so a structured rule sees the line with its noise already gone.
        self.structured_key(s).unwrap_or_else(|| s.to_string())
    }
}

// ------------------------------------------------------------ profile set ----

/// The built-in profiles, embedded so the binary works with no `profiles.d`
/// mounted, and overridable by a file of the same name in `profiles.d/`.
pub const BUILTIN_PROFILES: &[(&str, &str)] = &[
    ("raw", include_str!("../../../../profiles.d/raw.toml")),
    ("linux", include_str!("../../../../profiles.d/linux.toml")),
    ("uboot", include_str!("../../../../profiles.d/uboot.toml")),
    ("zephyr", include_str!("../../../../profiles.d/zephyr.toml")),
    ("uefi", include_str!("../../../../profiles.d/uefi.toml")),
    ("tfa", include_str!("../../../../profiles.d/tfa.toml")),
    ("optee", include_str!("../../../../profiles.d/optee.toml")),
    (
        "freertos",
        include_str!("../../../../profiles.d/freertos.toml"),
    ),
    (
        "threadx",
        include_str!("../../../../profiles.d/threadx.toml"),
    ),
    (
        "mcuboot",
        include_str!("../../../../profiles.d/mcuboot.toml"),
    ),
    (
        "android",
        include_str!("../../../../profiles.d/android.toml"),
    ),
    (
        "cros-ec",
        include_str!("../../../../profiles.d/cros-ec.toml"),
    ),
];

#[derive(Debug, Default)]
pub struct ProfileSet {
    profiles: Vec<Arc<Profile>>,
}

impl ProfileSet {
    /// Built-ins only.
    pub fn builtin() -> Result<Self> {
        let mut set = ProfileSet::default();
        for (name, toml) in BUILTIN_PROFILES {
            let p = Profile::from_toml(toml).map_err(|e| {
                ToolError::new(
                    ErrorCode::Internal,
                    format!("built-in profile {name} is broken: {}", e.message),
                )
            })?;
            set.insert(p);
        }
        Ok(set)
    }

    /// Built-ins plus every `*.toml` in `dir`, where a file may replace a
    /// built-in by using its name.
    pub fn load(dir: Option<&Path>) -> Result<Self> {
        let mut set = Self::builtin()?;
        let Some(dir) = dir else { return Ok(set) };
        if !dir.is_dir() {
            return Ok(set);
        }
        let mut entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        entries.sort();
        for path in entries {
            let text = std::fs::read_to_string(&path)?;
            let p = Profile::from_toml(&text).map_err(|e| {
                ToolError::new(
                    ErrorCode::InvalidConfig,
                    format!("{}: {}", path.display(), e.message),
                )
            })?;
            set.insert(p);
        }
        Ok(set)
    }

    pub fn insert(&mut self, p: Profile) {
        let p = Arc::new(p);
        match self.profiles.iter().position(|x| x.name == p.name) {
            Some(i) => self.profiles[i] = p,
            None => self.profiles.push(p),
        }
        // Stage detection walks profiles in boot order.
        self.profiles.sort_by(|a, b| {
            a.stage_rank
                .cmp(&b.stage_rank)
                .then_with(|| a.name.cmp(&b.name))
        });
    }

    pub fn get(&self, name: &str) -> Option<Arc<Profile>> {
        self.profiles.iter().find(|p| p.name == name).cloned()
    }

    pub fn require(&self, name: &str) -> Result<Arc<Profile>> {
        self.get(name).ok_or_else(|| {
            ToolError::new(
                ErrorCode::InvalidArgument,
                format!("no profile named {name:?}"),
            )
            .with_detail(serde_json::json!({ "available": self.names() }))
        })
    }

    pub fn names(&self) -> Vec<&str> {
        self.profiles.iter().map(|p| p.name.as_str()).collect()
    }

    pub fn all(&self) -> &[Arc<Profile>] {
        &self.profiles
    }

    /// Profiles that can claim a stage transition — everything except `raw`
    /// (the fallback) and overlays (which never replace the active dialect).
    pub fn detectors(&self) -> impl Iterator<Item = &Arc<Profile>> {
        self.profiles.iter().filter(|p| p.name != "raw")
    }
}

// --------------------------------------------------------- profile framer ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Between records.
    Idle,
    /// Inside a crash record's context dump.
    Context,
    /// Inside a backtrace.
    Backtrace,
}

#[derive(Debug)]
struct Held {
    line_id: i64,
    /// Display view, kept so a retro-attached line reads as it arrived.
    text: String,
    ts: i64,
}

#[derive(Debug)]
struct Open {
    /// The profile whose grammar continues and closes this record. An overlay
    /// record keeps its own rules even though the stage never changed.
    owner: Arc<Profile>,
    first_line_id: i64,
    last_line_id: i64,
    lines: Vec<String>,
    kind: RecordKind,
    severity: Severity,
    stage: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
    mine_key: String,
    opened_at: i64,
    last_at: i64,
    truncated: bool,
    retro_attached: usize,
    interleave_suspected: bool,
    /// Opened by a trigger that is *itself* a lookback line — a fault-decode
    /// preamble waiting for the real fatal banner (Zephyr's Cortex-M decode
    /// prints before `>>> ZEPHYR FATAL ERROR`). The next lookback trigger
    /// upgrades this record rather than nesting a second one.
    preamble: bool,
}

struct TrigInfo {
    severity: Severity,
    kind: RecordKind,
    lookback: bool,
}

/// Seal an open record, stamping the framer flags an agent needs to judge how
/// much to trust it: what closed it, whether lines were retro-attached, and
/// whether interleaved output was suspected (§11).
fn finish(mut o: Open, closed_by: &str) -> FramedRecord {
    o.fields.insert(
        "closed_by".into(),
        serde_json::Value::String(closed_by.into()),
    );
    if o.retro_attached > 0 {
        o.fields.insert(
            "retro_attached".into(),
            serde_json::Value::from(o.retro_attached),
        );
    }
    if o.interleave_suspected {
        o.fields
            .insert("interleave_suspected".into(), serde_json::Value::Bool(true));
    }
    o.fields.insert(
        "duration_ms".into(),
        serde_json::Value::from(o.last_at - o.opened_at),
    );
    let line_count = o.lines.len() as i64;
    FramedRecord {
        first_line_id: o.first_line_id,
        last_line_id: o.last_line_id,
        line_count,
        kind: o.kind,
        severity: o.severity,
        profile: o.owner.name.clone(),
        stage: o.stage,
        truncated: o.truncated,
        fields: serde_json::Value::Object(o.fields),
        mine_key: o.kind.is_minable().then(|| o.mine_key.clone()),
        text: o.lines.join("\n"),
    }
}

/// Framer driven by a compiled profile plus a stage machine.
#[derive(Debug)]
pub struct ProfileFramer {
    set: Arc<ProfileSet>,
    stages: super::StageMachine,
    active: Arc<Profile>,
    state: State,
    open: Option<Open>,
    lookback: VecDeque<Held>,
    lookback_depth: usize,
    max_record_lines: usize,
    record_timeout_ms: i64,
    garbage_run: Option<Open>,
}

impl ProfileFramer {
    pub fn new(
        set: Arc<ProfileSet>,
        cfg: &crate::config::FramerConfig,
        pinned: Option<&str>,
    ) -> Result<Self> {
        let stages = super::StageMachine::new(set.clone(), pinned)?;
        let active = stages.active_profile();
        Ok(Self {
            set,
            stages,
            active,
            state: State::Idle,
            open: None,
            lookback: VecDeque::new(),
            lookback_depth: cfg.lookback_lines,
            max_record_lines: cfg.max_record_lines,
            record_timeout_ms: cfg.record_timeout_s as i64 * 1000,
            garbage_run: None,
        })
    }

    fn timeout_ms(&self) -> i64 {
        self.active
            .record_timeout_s
            .map(|s| s as i64 * 1000)
            .unwrap_or(self.record_timeout_ms)
    }

    fn close(&mut self, out: &mut Vec<FramerEvent>, closed_by: &str) {
        if let Some(o) = self.open.take() {
            out.push(FramerEvent::Record(Box::new(finish(o, closed_by))));
        }
        self.state = State::Idle;
    }

    /// Emit lines held for possible retro-attachment as ordinary records.
    ///
    /// A lookback-eligible line is *withheld* rather than emitted, because if a
    /// trigger follows it belongs inside that record — emitting it first and
    /// absorbing it later would cover the same line twice and break the framer
    /// conservation property (§12.1). When no trigger follows, the held lines
    /// are released here, in order, exactly once.
    fn release_lookback(&mut self, out: &mut Vec<FramerEvent>) {
        while let Some(h) = self.lookback.pop_front() {
            out.push(FramerEvent::Record(Box::new(FramedRecord {
                first_line_id: h.line_id,
                last_line_id: h.line_id,
                line_count: 1,
                kind: RecordKind::Line,
                severity: self.active.severity_of(&h.text),
                profile: self.active.name.clone(),
                stage: Some(self.stages.stage().to_string()),
                truncated: false,
                fields: serde_json::Value::Object(self.active.extract_fields(&h.text)),
                mine_key: Some(self.active.mine_key(&h.text)),
                text: h.text,
            })));
        }
    }

    fn close_garbage(&mut self, out: &mut Vec<FramerEvent>) {
        if let Some(o) = self.garbage_run.take() {
            out.push(FramerEvent::Record(Box::new(finish(o, "run_ended"))));
        }
    }

    fn view(&self, text: &str) -> String {
        if self.active.ansi_strip {
            strip_ansi(text).into_owned()
        } else {
            text.to_string()
        }
    }

    fn open_record(
        &mut self,
        owner: Arc<Profile>,
        line: &FramerInput,
        view: &str,
        key: &str,
        trig: TrigInfo,
    ) {
        let mut lines = Vec::new();
        let mut first = line.line_id;
        let mut retro = 0usize;

        if trig.lookback {
            // Retro-attach the held lines (Zephyr's Cortex-M fault decode, a
            // ThreadX SCB dump, a Python traceback's frames). Everything in the
            // buffer is eligible by construction, and it is a contiguous run
            // because any non-eligible line released it.
            if let Some(h) = self.lookback.front() {
                first = h.line_id;
            }
            retro = self.lookback.len();
            lines.extend(self.lookback.iter().map(|h| h.text.clone()));
        }
        lines.push(view.to_string());

        let fields = owner.extract_fields(view);
        let preamble = owner.is_lookback(key);
        self.open = Some(Open {
            owner,
            first_line_id: first,
            last_line_id: line.line_id,
            lines,
            kind: trig.kind,
            severity: trig.severity,
            stage: Some(self.stages.stage().to_string()),
            fields,
            mine_key: key.to_string(),
            opened_at: line.ts_wall,
            last_at: line.ts_wall,
            truncated: false,
            retro_attached: retro,
            interleave_suspected: false,
            preamble,
        });
        self.state = State::Context;
        self.lookback.clear();
    }

    /// Emit a line as its own single-line record under `owner`'s attribution.
    fn single(&self, owner: &Arc<Profile>, line_id: i64, view: String) -> FramerEvent {
        let fields = owner.extract_fields(&view);
        let severity = owner.severity_of(&view);
        let mine_key = owner.mine_key(&view);
        FramerEvent::Record(Box::new(FramedRecord {
            first_line_id: line_id,
            last_line_id: line_id,
            line_count: 1,
            kind: RecordKind::Line,
            severity,
            profile: owner.name.clone(),
            stage: Some(self.stages.stage().to_string()),
            truncated: false,
            fields: serde_json::Value::Object(fields),
            mine_key: Some(mine_key),
            text: view,
        }))
    }
}

impl Framer for ProfileFramer {
    fn name(&self) -> &str {
        &self.active.name
    }

    fn stage(&self) -> Option<&str> {
        Some(self.stages.stage())
    }

    fn push(&mut self, line: FramerInput) -> Vec<FramerEvent> {
        let mut out = Vec::new();

        // Garbage and binary spans never reach the grammar: they are quarantined
        // as their own records so a baud mismatch or a Sahara transfer cannot
        // pollute templates or open a bogus crash record.
        if line.garbage || line.binary {
            self.close(&mut out, "garbage");
            self.release_lookback(&mut out);
            let kind = if line.binary {
                RecordKind::Binary
            } else {
                RecordKind::Garbage
            };
            match self.garbage_run.as_mut() {
                Some(g) if g.kind == kind => {
                    g.last_line_id = line.line_id;
                    g.last_at = line.ts_wall;
                    g.lines.push(line.text.clone());
                }
                _ => {
                    self.close_garbage(&mut out);
                    self.garbage_run = Some(Open {
                        owner: self.active.clone(),
                        first_line_id: line.line_id,
                        last_line_id: line.line_id,
                        lines: vec![line.text.clone()],
                        kind,
                        severity: Severity::Warn,
                        stage: Some(self.stages.stage().to_string()),
                        fields: serde_json::Map::new(),
                        mine_key: String::new(),
                        opened_at: line.ts_wall,
                        last_at: line.ts_wall,
                        truncated: false,
                        retro_attached: 0,
                        interleave_suspected: false,
                        preamble: false,
                    });
                }
            }
            return out;
        }
        self.close_garbage(&mut out);

        // Two views of the same line, with the raw bytes untouched behind both:
        //
        //   `view` — display text (ANSI stripped if the profile asks). Severity
        //            prefixes and field extraction need it intact, and banner
        //            detection reads it because a banner announces the *next*
        //            dialect and cannot be matched through the current one.
        //   `key`  — `view` with the owning profile's already-extracted spans
        //            removed (a printk `[    3.221030] ` stamp, an OP-TEE
        //            `E/TC:` prefix). Every *grammar* rule matches this.
        let view = self.view(&line.text);

        // An overlay dialect claims individual lines without taking the stage:
        // OP-TEE core messages inside Linux output, `(XEN)` over dom0.
        let overlay: Option<Arc<Profile>> = self
            .set
            .all()
            .iter()
            .find(|p| p.overlay && p.name != self.active.name && p.claims(&view))
            .cloned();

        // An open record keeps its own grammar until it closes, so an overlay
        // crash is framed by the overlay's rules even mid-kernel-boot.
        if let Some(owner) = self.open.as_ref().map(|o| o.owner.clone()) {
            let key = owner.mine_key(&view);

            // A line that is both a terminator and a reset marker — U-Boot's
            // "resetting ...", ESP-IDF's "Rebooting..." — belongs inside the
            // record it ends, and still opens an epoch.
            if owner.is_terminator(&key) {
                if let Some(o) = self.open.as_mut() {
                    o.lines.push(view.clone());
                    o.last_line_id = line.line_id;
                    o.last_at = line.ts_wall;
                }
                self.close(&mut out, "terminator");
                if let Some(t) = self.stages.observe(&view, line.line_id, line.ts_wall) {
                    self.active = self.stages.active_profile();
                    out.push(FramerEvent::Stage(t));
                }
                return out;
            }

            if let Some(t) = owner.trigger(&key) {
                // The fatal banner arriving after its own fault-decode preamble
                // is one crash, not two: absorb, and take the banner as the
                // record's mining key so the template names the real event.
                if t.lookback && self.open.as_ref().is_some_and(|o| o.preamble) {
                    let fields = owner.extract_fields(&view);
                    let o = self.open.as_mut().expect("checked");
                    o.lines.push(view);
                    o.last_line_id = line.line_id;
                    o.last_at = line.ts_wall;
                    o.severity = o.severity.min(t.severity);
                    o.kind = t.kind;
                    o.mine_key = key;
                    o.preamble = false;
                    o.fields.extend(fields);
                    return out;
                }
                // Otherwise a trigger inside an open record is a panic-inside-
                // panic: close the outer one honestly rather than swallowing it.
                let info = TrigInfo {
                    severity: t.severity,
                    kind: t.kind,
                    lookback: t.lookback,
                };
                self.close(&mut out, "nested_trigger");
                self.open_record(owner, &line, &view, &key, info);
                return out;
            }

            let continues = owner.is_continuation(&key)
                || owner.is_context(&key)
                || owner.is_backtrace_header(&key)
                || owner.is_backtrace_frame(&key);
            if continues {
                if owner.is_backtrace_header(&key) {
                    self.state = State::Backtrace;
                }
                let interleaved = self.state == State::Backtrace && !owner.is_backtrace_frame(&key);
                let over_cap = {
                    let o = self.open.as_mut().expect("open checked");
                    o.lines.push(view);
                    o.last_line_id = line.line_id;
                    o.last_at = line.ts_wall;
                    // Interleaved SMP oops lines arrive out of order; v1 frames
                    // greedily and flags the suspicion rather than reassembling.
                    o.interleave_suspected |= interleaved;
                    o.lines.len() >= self.max_record_lines
                };
                if over_cap {
                    if let Some(o) = self.open.as_mut() {
                        o.truncated = true;
                    }
                    self.close(&mut out, "max_record_lines");
                }
                return out;
            }

            // Unrelated traffic: the record is over.
            self.close(&mut out, "unrelated_line");
        }

        // No record open. Stage transitions come first — a banner means the
        // dialect changed underneath us.
        if let Some(t) = self.stages.observe(&view, line.line_id, line.ts_wall) {
            self.release_lookback(&mut out);
            self.active = self.stages.active_profile();
            let owner = self.active.clone();
            out.push(FramerEvent::Stage(t));
            out.push(self.single(&owner, line.line_id, view));
            return out;
        }

        let owner = overlay.unwrap_or_else(|| self.active.clone());
        let key = owner.mine_key(&view);

        if let Some(t) = owner.trigger(&key) {
            let info = TrigInfo {
                severity: t.severity,
                kind: t.kind,
                lookback: t.lookback,
            };
            if !t.lookback {
                // Nothing to retro-attach: release the held lines before the
                // record opens, so they keep their stream order.
                self.release_lookback(&mut out);
            }
            self.open_record(owner, &line, &view, &key, info);
            return out;
        }

        // A line the profile marks as retro-attachable is withheld, not emitted:
        // if a trigger follows it belongs to that record.
        if owner.is_lookback(&key) {
            self.lookback.push_back(Held {
                line_id: line.line_id,
                text: view,
                ts: line.ts_wall,
            });
            if self.lookback.len() > self.lookback_depth {
                // Bounded by `framer.lookback_lines`: the oldest held line is
                // released as its own record, never dropped.
                let h = self.lookback.pop_front().expect("over depth");
                out.push(self.single(&self.active.clone(), h.line_id, h.text));
            }
            return out;
        }

        self.release_lookback(&mut out);
        out.push(self.single(&owner, line.line_id, view));
        out
    }

    fn tick(&mut self, now_ms: i64) -> Vec<FramerEvent> {
        let mut out = Vec::new();
        let timeout = self.timeout_ms();
        // A held line with no trigger behind it must not stay invisible.
        if self
            .lookback
            .back()
            .is_some_and(|h| now_ms - h.ts >= timeout)
        {
            self.release_lookback(&mut out);
        }
        if let Some(o) = &self.open {
            if now_ms - o.last_at >= timeout {
                // DEAD_AIR: UEFI's `CpuDeadLoop` and TF-A's post-panic spin never
                // print a terminator, so silence is the close.
                self.close(&mut out, "dead_air");
            }
        }
        if let Some(g) = &self.garbage_run {
            if now_ms - g.last_at >= timeout {
                self.close_garbage(&mut out);
            }
        }
        out
    }

    fn flush(&mut self) -> Vec<FramerEvent> {
        let mut out = Vec::new();
        self.release_lookback(&mut out);
        if let Some(o) = self.open.as_mut() {
            // Power loss mid-record: the record is real, and saying so is more
            // useful than dropping it.
            o.truncated = true;
        }
        self.close(&mut out, "stream_end");
        self.close_garbage(&mut out);
        out
    }
}

impl ProfileFramer {
    /// Prompt patterns the active profile expects, for `get_prompts` (§8.5).
    pub fn prompts(&self) -> &[PromptPattern] {
        &self.active.prompts
    }

    pub fn active_profile(&self) -> &Arc<Profile> {
        &self.active
    }

    pub fn stage_machine(&self) -> &super::StageMachine {
        &self.stages
    }

    /// Lines currently buffered for retro-attachment, oldest first.
    pub fn lookback_len(&self) -> usize {
        self.lookback.len()
    }

    /// Timestamp of the newest lookback line, for silence accounting.
    pub fn last_seen(&self) -> Option<i64> {
        self.lookback.back().map(|h| h.ts)
    }
}
