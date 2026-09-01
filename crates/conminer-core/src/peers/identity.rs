//! Who this node is, and why it stays that way.
//!
//! A fleet member needs two names: an INSTANCE ID that never changes, and a
//! human NAME that anyone may change. Conflating them is how a renamed node
//! becomes a second ghost node in everyone else's peer table, and how a
//! re-imaged host silently inherits another one's device rows.

use crate::error::{ErrorCode, Result, ToolError};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Identity {
    /// Stable across restarts, renames and config edits.
    pub instance_id: String,
    /// What humans and agents type. Overridable in `[peers] name`.
    pub name: String,
    pub created_at: i64,
}

impl Identity {
    /// Load the persisted identity, creating it on first run.
    ///
    /// Written atomically (temp file + rename): a half-written identity file
    /// after a power cut would make this node anonymous to the fleet, and an
    /// anonymous node is one whose devices nobody can address.
    pub fn load_or_create(data_dir: &Path, configured_name: &str, now: i64) -> Result<Self> {
        let path = data_dir.join("instance.json");
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut id) = serde_json::from_str::<Identity>(&text) {
                // A configured name wins for DISPLAY, but never rewrites the id.
                if !configured_name.is_empty() && id.name != configured_name {
                    id.name = configured_name.to_string();
                    write_atomically(&path, &id)?;
                }
                return Ok(id);
            }
        }
        let id = Identity {
            instance_id: uuid_v4(now),
            name: if configured_name.is_empty() {
                friendly_name(now)
            } else {
                configured_name.to_string()
            },
            created_at: now,
        };
        std::fs::create_dir_all(data_dir).ok();
        write_atomically(&path, &id)?;
        Ok(id)
    }
}

fn write_atomically(path: &Path, id: &Identity) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(id).unwrap_or_default();
    std::fs::write(&tmp, text).map_err(|e| {
        ToolError::new(
            ErrorCode::Internal,
            format!("cannot write {}: {e}", tmp.display()),
        )
    })?;
    std::fs::rename(&tmp, path).map_err(|e| {
        ToolError::new(
            ErrorCode::Internal,
            format!("cannot install {}: {e}", path.display()),
        )
    })?;
    Ok(())
}

/// A uuid4-shaped id from the system's randomness.
///
/// Hand-rolled for the same reason the HTTP client is: the dependency budget in
/// this project buys capture, not conveniences. `getrandom` via /dev/urandom
/// with a time-seeded fallback, then formatted per RFC 4122 §4.4.
fn uuid_v4(now: i64) -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_err()
    {
        // Never silently produce a constant: mix the clock and the pid so two
        // nodes that both lost /dev/urandom still differ.
        let seed = (now as u128) ^ ((std::process::id() as u128) << 64);
        for (i, byte) in b.iter_mut().enumerate() {
            *byte = ((seed >> (i * 4)) & 0xff) as u8;
        }
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h[0],
        h[1],
        h[2],
        h[3],
        h[4],
        h[5],
        h[6],
        h[7],
        h[8],
        h[9],
        h[10],
        h[11],
        h[12],
        h[13],
        h[14],
        h[15]
    )
}

/// `adjective-animal-NN`: readable in a log line, typeable in a selector.
fn friendly_name(now: i64) -> String {
    const ADJECTIVES: [&str; 12] = [
        "brisk", "calm", "dusky", "eager", "fair", "keen", "lively", "mellow", "nimble", "quiet",
        "swift", "warm",
    ];
    const ANIMALS: [&str; 12] = [
        "otter", "heron", "lynx", "marten", "osprey", "raven", "shrike", "stoat", "tern", "vole",
        "wren", "ibex",
    ];
    let mut b = [0u8; 3];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_err()
    {
        b = [(now & 0xff) as u8, ((now >> 8) & 0xff) as u8, 0];
    }
    format!(
        "{}-{}-{:02}",
        ADJECTIVES[b[0] as usize % ADJECTIVES.len()],
        ANIMALS[b[1] as usize % ANIMALS.len()],
        b[2] % 100
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::load_or_create(dir.path(), "", 1000).unwrap();
        let b = Identity::load_or_create(dir.path(), "", 2000).unwrap();
        assert_eq!(a, b, "a second start must not mint a second identity");
        assert_eq!(a.instance_id.len(), 36, "uuid shape: {}", a.instance_id);
        assert_eq!(&a.instance_id[14..15], "4", "version nibble");
    }

    #[test]
    fn renaming_a_node_does_not_make_it_a_new_node() {
        let dir = tempfile::tempdir().unwrap();
        let first = Identity::load_or_create(dir.path(), "spike", 1000).unwrap();
        let renamed = Identity::load_or_create(dir.path(), "alpha", 2000).unwrap();
        assert_eq!(
            first.instance_id, renamed.instance_id,
            "the id is what peers match on; a rename that changed it would leave a ghost \
             node in every other node's table"
        );
        assert_eq!(renamed.name, "alpha");
    }

    #[test]
    fn two_nodes_get_different_ids() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let one = Identity::load_or_create(a.path(), "", 1000).unwrap();
        let two = Identity::load_or_create(b.path(), "", 1000).unwrap();
        assert_ne!(one.instance_id, two.instance_id);
    }
}
