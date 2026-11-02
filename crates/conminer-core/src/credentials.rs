//! Per-device console login credentials (§8.5, `credentials.file`).
//!
//! The file is the one place a console password lives. It is read on demand
//! and only when a console is actually sitting at a credential gate, so a
//! broken or insecure file never stops `run_command` at an ordinary shell.
//!
//! Format (TOML), keyed exactly like `[devices."<selector>"]` in conminer.toml:
//! the port path or the operator's label.
//!
//! ```toml
//! [devices."/dev/serial/by-id/usb-FTDI_Example-if01-port0"]
//! user = "root"
//! password = "..."
//! ```
//!
//! The file is refused unless it is a regular file of mode 0600 owned by the
//! uid this process runs as. A password is held in [`Secret`], which has no
//! `Serialize` and whose `Debug` prints the redaction marker, so no response,
//! store row or log line can carry it by accident.

use crate::error::{ErrorCode, Result, ToolError};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// What a password is recorded as wherever the transaction is recorded.
pub const MARKER: &str = "<credential>";

/// A password. Deliberately not `Serialize`, and `Debug` never shows it.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The bytes to type. The only accessor; callers send it and drop it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(MARKER)
    }
}

/// One device's login.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub user: String,
    #[serde(default = "empty_secret")]
    pub password: Secret,
}

fn empty_secret() -> Secret {
    Secret::new("")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileFormat {
    #[serde(default)]
    devices: BTreeMap<String, Credential>,
}

/// The parsed file.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    devices: BTreeMap<String, Credential>,
}

impl Credentials {
    /// The login for a console, matched by any of its names (port path first,
    /// then label), the same rule as the per-device overrides in conminer.toml.
    pub fn for_device(&self, keys: &[&str]) -> Option<&Credential> {
        keys.iter()
            .filter(|k| !k.is_empty())
            .find_map(|k| self.devices.get(*k))
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

/// Why a configured file was not used. Never carries file content.
fn refused(path: &Path, why: impl Into<String>) -> ToolError {
    ToolError::new(
        ErrorCode::InvalidConfig,
        format!(
            "credentials file {} refused: {}",
            path.display(),
            why.into()
        ),
    )
    .with_hint(
        "credentials.file must be a regular file of mode 0600 owned by the user conminer \
         runs as; see docs/credentials.md",
    )
}

/// Load and check the file. The checks are made on the opened descriptor, so
/// the file judged is the file read.
pub fn load(path: &Path) -> Result<Credentials> {
    load_as(path, effective_uid())
}

/// [`load`] against an explicit owner, so the owner rule is testable without
/// running the suite as another user.
pub fn load_as(path: &Path, uid: u32) -> Result<Credentials> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| refused(path, e.to_string()))?;
    let meta = f.metadata().map_err(|e| refused(path, e.to_string()))?;
    if !meta.file_type().is_file() {
        return Err(refused(path, "not a regular file"));
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode != 0o600 {
        return Err(refused(path, format!("mode {mode:04o}, want 0600")));
    }
    if meta.uid() != uid {
        return Err(refused(
            path,
            format!("owned by uid {}, want uid {uid}", meta.uid()),
        ));
    }
    let mut text = String::new();
    f.read_to_string(&mut text)
        .map_err(|e| refused(path, e.to_string()))?;
    // A TOML error message quotes the offending source line, which in this
    // file is a password. Report only where it is.
    let parsed: FileFormat = toml::from_str(&text).map_err(|e| {
        let line = e
            .span()
            .map(|s| text[..s.start.min(text.len())].matches('\n').count() + 1);
        refused(
            path,
            match line {
                Some(n) => format!("not valid credentials TOML (line {n})"),
                None => "not valid credentials TOML".to_string(),
            },
        )
    })?;
    Ok(Credentials {
        devices: parsed.devices,
    })
}

/// The uid this process runs as.
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// What a console login resolved to before the runner ever sees the gate.
#[derive(Debug, Clone, Default)]
pub enum Login {
    /// No file configured, or no entry for this console.
    #[default]
    None,
    /// A file is configured and was refused; the reason, never its content.
    Refused(String),
    Use(Credential),
}

impl Login {
    /// Resolve the login for a console from the configured file.
    pub fn resolve(file: &str, keys: &[&str]) -> Self {
        if file.is_empty() {
            return Login::None;
        }
        match load(Path::new(file)) {
            Ok(c) => match c.for_device(keys) {
                Some(cred) => Login::Use(cred.clone()),
                None => Login::None,
            },
            Err(e) => Login::Refused(e.message),
        }
    }
}

/// Refuse a host path that is the credentials file, whatever name it is
/// reached by (a symlink, a hard link, `..`). Tools that read or write host
/// files would otherwise copy the password into the store, an export, or a
/// board.
pub fn guard_path(file: &str, candidate: &Path) -> Result<()> {
    if file.is_empty() {
        return Ok(());
    }
    let creds = PathBuf::from(file);
    let same_inode = match (std::fs::metadata(&creds), std::fs::metadata(candidate)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    let same_name = match (
        std::fs::canonicalize(&creds),
        std::fs::canonicalize(candidate),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => creds == candidate,
    };
    if same_inode || same_name {
        return Err(ToolError::new(
            ErrorCode::PermissionDenied,
            format!("{} is the credentials file", candidate.display()),
        )
        .with_hint("console credentials never leave the credentials file"));
    }
    Ok(())
}

/// Replace every occurrence of `secret` in `s` with the marker.
pub fn redact(s: &str, secret: &Secret) -> String {
    if secret.is_empty() || !s.contains(secret.expose()) {
        return s.to_string();
    }
    s.replace(secret.expose(), MARKER)
}

/// [`redact`] applied to every string inside a JSON value.
pub fn redact_value(v: &mut serde_json::Value, secret: &Secret) {
    use serde_json::Value;
    match v {
        Value::String(s) => {
            if !secret.is_empty() && s.contains(secret.expose()) {
                *s = redact(s, secret);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| redact_value(x, secret)),
        Value::Object(m) => m.values_mut().for_each(|x| redact_value(x, secret)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_prints() {
        let c = Credential {
            user: "root".into(),
            password: Secret::new("hunter2"),
        };
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("hunter2"), "{dbg}");
        assert!(dbg.contains(MARKER), "{dbg}");
    }

    #[test]
    fn redaction_reaches_nested_json() {
        let s = Secret::new("hunter2");
        let mut v = serde_json::json!({"a": ["x hunter2 y", {"b": "hunter2"}], "c": 1});
        redact_value(&mut v, &s);
        let t = v.to_string();
        assert!(!t.contains("hunter2"), "{t}");
        assert_eq!(t.matches(MARKER).count(), 2, "{t}");
    }

    #[test]
    fn an_empty_secret_redacts_nothing() {
        assert_eq!(redact("abc", &Secret::new("")), "abc");
    }
}
