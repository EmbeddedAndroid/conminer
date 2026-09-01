//! The fleet table: who is out there, and how stale is that belief.
//!
//! SOFT STATE WITH A TTL, not a membership protocol. A peer exists because it
//! said so recently; it stops existing because it stopped saying so. There is no
//! election, no quorum and nothing to un-wedge at 3am -- the failure mode of a
//! consensus layer on a two-node bench is worse than the problem it solves.
//!
//! It lives in registry.db rather than in peerd's memory because mcpd and dashd
//! are separate processes that both need the answer, and a table they can both
//! read is simpler than a third socket between them.

use crate::error::{ErrorCode, Result, ToolError};
use crate::store::Registry;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerSource {
    /// Heard on the UDP beacon.
    Beacon,
    /// Heard over mDNS.
    Mdns,
    /// Named in `[peers] nodes`. Never expires: it is a statement of intent by
    /// the operator, and it is the only discovery that crosses a subnet that
    /// broadcast cannot (a NAT-ed host talking to a lab host, for one).
    Static,
    /// §P3. The peer announced ITSELF to us, over a connection it opened.
    ///
    /// The only way a node behind one-way connectivity learns the fleet:
    /// inventory is a pull, so a node that can reach nobody sees nobody however
    /// many peers can reach it. Measured on the bravo bench, which sits upstream
    /// of a NAT and could not open a connection to any other node.
    Push,
}

impl PeerSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Beacon => "beacon",
            Self::Mdns => "mdns",
            Self::Static => "static",
            Self::Push => "push",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "mdns" => Self::Mdns,
            "static" => Self::Static,
            "push" => Self::Push,
            _ => Self::Beacon,
        }
    }
    pub fn never_expires(&self) -> bool {
        matches!(self, Self::Static)
    }

    /// Can this node dial the peer back?
    ///
    /// A pushed peer reached US; that says nothing about the return path, and
    /// the difference decides whether its boards can be actuated from here or
    /// only watched. Saying so is the whole point -- a call that cannot arrive
    /// should fail with a reason, not a timeout.
    pub fn is_push(&self) -> bool {
        matches!(self, Self::Push)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRow {
    pub instance_id: String,
    pub name: String,
    /// The address this peer is reachable on -- what makes a remote asset
    /// unambiguous in a listing: "adp-ventuno on alpha (192.168.10.10)".
    pub host: Option<String>,
    pub mcp_url: String,
    pub dash_url: Option<String>,
    pub ser2net_host: Option<String>,
    pub version: Option<String>,
    pub source: PeerSource,
    /// Did the last inventory sync succeed? A peer can be ADVERTISING and still
    /// be failing to answer, and those are different problems.
    pub ok: bool,
    pub last_seen: i64,
    pub last_error: Option<String>,
    pub advert_count: i64,
    /// §P3. When this peer last asked us for work over the reverse channel.
    /// `None` means never. Together with `ok` it says which way the wire opens.
    pub last_poll: Option<i64>,
}

impl PeerRow {
    /// Is this peer still believed live?
    pub fn is_live(&self, now: i64, ttl_s: u64) -> bool {
        self.source.never_expires() || (now - self.last_seen) <= (ttl_s as i64) * 1000
    }

    pub fn age_ms(&self, now: i64) -> i64 {
        (now - self.last_seen).max(0)
    }
}

/// What a beacon or an mDNS record carries. Also the shape peerd advertises.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Advert {
    pub instance_id: String,
    pub name: String,
    pub version: String,
    pub mcp_url: String,
    pub dash_url: String,
    pub ser2net_host: String,
    #[serde(default)]
    pub ser2net_ports: Vec<u16>,
}

/// Upsert a peer from an advert.
///
/// `advert_count` climbing is what tells an operator the link is healthy rather
/// than merely once-seen; a flapping peer shows a stalled count and a young
/// `last_seen` at the same time.
pub fn upsert_advert(
    reg: &mut Registry,
    advert: &Advert,
    source: PeerSource,
    host: Option<&str>,
    now: i64,
) -> Result<()> {
    reg.conn().execute(
        "INSERT INTO peers(instance_id,name,host,mcp_url,dash_url,ser2net_host,version,source,
                           ok,last_seen,advert_count)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?10,?9,1)
         ON CONFLICT(instance_id) DO UPDATE SET
            name=?2, host=?3, mcp_url=?4, dash_url=?5, ser2net_host=?6, version=?7,
            -- A static entry that is also heard on the wire stays static: the
            -- operator's statement outranks a beacon that may stop.
            source=CASE WHEN peers.source='static' THEN 'static' ELSE ?8 END,
            last_seen=?9,
            advert_count=peers.advert_count+1",
        rusqlite::params![
            advert.instance_id,
            advert.name,
            host,
            advert.mcp_url,
            advert.dash_url,
            advert.ser2net_host,
            advert.version,
            source.as_str(),
            now,
            // §P3. AN ANNOUNCEMENT IS EVIDENCE OF THE OTHER DIRECTION.
            //
            // `ok` means "we can reach it", and a beacon or a static entry starts
            // optimistic because we are about to go and find out. A push does the
            // opposite: the peer reached US, which says nothing about the way back
            // and is very often proof that there isn't one. Starting it at "yes"
            // would have the router dial an address it cannot reach and spend a
            // caller's whole budget learning that. Unknown is the honest value,
            // and the router's fallback for unknown -- relay if it is listening --
            // is the one that works.
            !source.is_push(),
        ],
    )?;
    Ok(())
}

/// Record a static peer before anything is known about it but its URL.
///
/// The id is a placeholder until the peer answers and tells us its real one;
/// `adopt_identity` then merges the placeholder away.
pub fn upsert_static(reg: &mut Registry, url: &str, now: i64) -> Result<()> {
    // DO NOT RE-INVENT A STAND-IN FOR A NODE WE HAVE ALREADY MET.
    //
    // `adopt_identity` drops the placeholder the first time the peer answers.
    // This runs again on every startup, and it used to recreate one
    // unconditionally -- so the moment the peer went down, the stand-in came
    // back and stayed, because only a SUCCESSFUL probe can clear it. Measured on
    // alpha: charlie listed twice, once truthfully as `Connection refused` and
    // once as a node literally named `http://192.168.10.12:8090/mcp`.
    //
    // Keyed on the URL, which is all a static entry knows: if any non-placeholder
    // row already claims it, that row IS this peer and there is nothing to stand
    // in for.
    let known: Option<i64> = reg
        .conn()
        .query_row(
            "SELECT 1 FROM peers
              WHERE mcp_url = ?1
                AND instance_id NOT LIKE 'static:%'
                AND instance_id NOT LIKE 'static-id:%'
              LIMIT 1",
            rusqlite::params![url],
            |r| r.get(0),
        )
        .optional()
        .map_err(crate::error::ToolError::from)?;
    if known.is_some() {
        // AND SWEEP UP THE ONE THIS FUNCTION ALREADY LEFT BEHIND. Refusing to
        // create another does nothing for the benches that have been restarting
        // with the old code for weeks; the stand-in is in their table now, and
        // it is the row an operator is looking at. Same predicate as
        // `drop_placeholders`, so a peer that has identified itself can only
        // ever hold one row.
        reg.conn().execute(
            "DELETE FROM peers
              WHERE mcp_url = ?1
                AND (instance_id LIKE 'static:%' OR instance_id LIKE 'static-id:%')",
            rusqlite::params![url],
        )?;
        return Ok(());
    }

    let placeholder = format!("static:{url}");
    reg.conn().execute(
        // `ok=0`: NOT YET ANSWERED. This was 1, which is a claim of health about
        // a node nothing has ever spoken to -- and the page reads `ok` as
        // "answering", so an unreachable host was drawn in good standing beside
        // its own failing row. A first successful probe sets it through
        // `mark_ok`, which is the only thing entitled to say so.
        "INSERT INTO peers(instance_id,name,mcp_url,source,ok,last_seen,advert_count)
         VALUES(?1,?2,?3,'static',0,?4,0)
         -- ...AND CORRECT ONE AN OLDER BUILD SEEDED AS HEALTHY. Without the
         -- `ok=0` here, a stand-in written by the old code keeps `ok=1` for
         -- ever: this runs at startup, and nothing else rewrites a row that is
         -- never successfully probed. Found on bravo, whose stand-in had
         -- advert_count=0, no last_poll and no last_error -- never contacted by
         -- anything -- while the page drew it as a host in good standing.
         --
         -- Safe for a peer that DOES answer without reporting an instance id
         -- (the one case where a stand-in legitimately survives contact): this
         -- only runs at startup, so it says \"we have not spoken to it since we
         -- came up\", which is true, and the next successful probe sets it back
         -- through `mark_ok`.
         ON CONFLICT(instance_id) DO UPDATE SET mcp_url=?3, source='static', ok=0",
        rusqlite::params![placeholder, url, url, now],
    )?;
    Ok(())
}

/// A static peer answered and named itself: replace the placeholder row.
/// Drop EVERY placeholder for a node, not just the one keyed on this URL.
///
/// A placeholder is a row we invented from `[peers] nodes` before the peer could
/// tell us who it is. Once it has -- by answering a probe, or (§P3) by
/// announcing itself -- any leftover stand-in is a second entry for one host,
/// and two entries sync the same devices in turn, each marking the other's rows
/// gone. Measured on the first two-host bring-up, where a lab host appeared
/// twice and its thirteen consoles flickered between live and gone.
fn drop_placeholders(tx: &rusqlite::Transaction<'_>, url: &str, name: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM peers
          WHERE instance_id = ?1
             OR ((instance_id LIKE 'static:%' OR instance_id LIKE 'static-id:%')
                 AND (mcp_url = ?2 OR name = ?3))",
        rusqlite::params![format!("static:{url}"), url, name],
    )?;
    Ok(())
}

/// §P3. The same cleanup, for a node that introduced ITSELF.
///
/// An operator's static entry is a URL and nothing else. On a one-way link we
/// never get to probe it, so the placeholder would sit there for ever beside the
/// real row the announcement created -- one host, listed twice, one of the two
/// permanently dead. Which is exactly the mess this clears after a probe.
pub fn adopt_announcement(reg: &mut Registry, url: &str, name: &str) -> Result<usize> {
    if url.is_empty() {
        return Ok(0);
    }
    let tx = reg
        .conn()
        .unchecked_transaction()
        .map_err(crate::error::ToolError::from)?;
    drop_placeholders(&tx, url, name)?;
    let n = tx.changes() as usize;
    tx.commit().map_err(crate::error::ToolError::from)?;
    Ok(n)
}

/// §P3. Record that this peer just asked us for work.
///
/// Soft state, in the table rather than in the mcpd process, because dashd runs
/// separately and needs it to tell a node that cannot be dialled but is fully
/// driveable from one whose mcpd has crashed. Keyed by NAME: the poller
/// identifies itself the same way it announced.
pub fn note_poll(reg: &mut Registry, name: &str, now: i64) -> Result<()> {
    reg.conn().execute(
        "UPDATE peers SET last_poll = ?2 WHERE name = ?1",
        rusqlite::params![name, now],
    )?;
    Ok(())
}

pub fn adopt_identity(
    reg: &mut Registry,
    url: &str,
    advert: &Advert,
    host: Option<&str>,
    now: i64,
) -> Result<()> {
    let tx = reg
        .conn()
        .unchecked_transaction()
        .map_err(crate::error::ToolError::from)?;
    drop_placeholders(&tx, url, &advert.name)?;
    tx.execute(
        "INSERT INTO peers(instance_id,name,host,mcp_url,dash_url,ser2net_host,version,source,
                           ok,last_seen,advert_count)
         VALUES(?1,?2,?3,?4,?5,?6,?7,'static',1,?8,1)
         ON CONFLICT(instance_id) DO UPDATE SET
            name=?2, host=?3, mcp_url=?4, dash_url=?5, ser2net_host=?6, version=?7,
            source='static', last_seen=?8, advert_count=peers.advert_count+1",
        rusqlite::params![
            advert.instance_id,
            advert.name,
            host,
            advert.mcp_url,
            advert.dash_url,
            advert.ser2net_host,
            advert.version,
            now,
        ],
    )?;
    tx.commit().map_err(crate::error::ToolError::from)?;
    Ok(())
}

/// Note that a peer failed to answer, without forgetting it.
///
/// Deliberately not a delete: a peer that is advertising but not answering is a
/// different fact from a peer that has gone, and an operator needs to tell them
/// apart (one is a crashed mcpd, the other is a switched-off host).
pub fn mark_failed(reg: &mut Registry, instance_id: &str, why: &str, now: i64) -> Result<()> {
    reg.conn().execute(
        "UPDATE peers SET ok=0, last_error=?2, last_seen=CASE WHEN source='static' THEN ?3
         ELSE last_seen END WHERE instance_id=?1",
        rusqlite::params![instance_id, why, now],
    )?;
    Ok(())
}

pub fn mark_ok(reg: &mut Registry, instance_id: &str, now: i64) -> Result<()> {
    reg.conn().execute(
        "UPDATE peers SET ok=1, last_error=NULL, last_seen=?2 WHERE instance_id=?1",
        rusqlite::params![instance_id, now],
    )?;
    Ok(())
}

pub fn all(reg: &Registry) -> Result<Vec<PeerRow>> {
    let mut st = reg.conn().prepare(
        "SELECT instance_id,name,host,mcp_url,dash_url,ser2net_host,version,source,ok,
                last_seen,last_error,advert_count,last_poll
         FROM peers ORDER BY name",
    )?;
    let rows = st.query_map([], |r| {
        Ok(PeerRow {
            instance_id: r.get(0)?,
            name: r.get(1)?,
            host: r.get(2)?,
            mcp_url: r.get(3)?,
            dash_url: r.get(4)?,
            ser2net_host: r.get(5)?,
            version: r.get(6)?,
            source: PeerSource::parse(&r.get::<_, String>(7)?),
            ok: r.get::<_, i64>(8)? != 0,
            last_seen: r.get(9)?,
            last_error: r.get(10)?,
            advert_count: r.get(11)?,
            last_poll: r.get(12)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Drop one peer row by id.
///
/// Used when a "peer" turns out to be THIS node: a config that names its own
/// host makes a node proxy its own calls back to itself and re-export its own
/// boards as remote ones, and no amount of retrying fixes that -- the row has to
/// go.
pub fn forget(reg: &mut Registry, instance_id: &str) -> Result<()> {
    reg.conn().execute(
        "DELETE FROM peers WHERE instance_id=?1",
        rusqlite::params![instance_id],
    )?;
    Ok(())
}

/// Live peers only, by the TTL rule.
pub fn live(reg: &Registry, now: i64, ttl_s: u64) -> Result<Vec<PeerRow>> {
    Ok(all(reg)?
        .into_iter()
        .filter(|p| p.is_live(now, ttl_s))
        .collect())
}

/// Resolve a node NAME, refusing to guess when more than one answers to it.
///
/// `devices.node` keys on the name, and the router turns a name into an address
/// for actuation. Taking the first match therefore means a power action aimed at
/// one host can land on another, silently, with a success response from the
/// wrong board. There is no safe way to pick: the operator meant a physical
/// machine, and the fleet no longer knows which one that is.
///
/// This is not hypothetical. A deploy that copied one node's `.env` onto the
/// others left three hosts all named `charlie`, every peer table listing what
/// looked like duplicates, and nothing anywhere saying a word about it.
pub fn by_name(reg: &Registry, name: &str) -> Result<Option<PeerRow>> {
    let matches: Vec<PeerRow> = all(reg)?.into_iter().filter(|p| p.name == name).collect();
    if matches.len() > 1 {
        let ids: Vec<&str> = matches.iter().map(|p| p.instance_id.as_str()).collect();
        // The URL, not the host: two nodes can share a host (containers, NAT)
        // and the URL is what the router would actually dial.
        let urls: Vec<&str> = matches.iter().map(|p| p.mcp_url.as_str()).collect();
        return Err(ToolError::new(
            ErrorCode::AmbiguousPeer,
            format!(
                "{} nodes answer to the name {name:?}: {} at {}",
                matches.len(),
                ids.join(", "),
                urls.join(", ")
            ),
        ));
    }
    Ok(matches.into_iter().next())
}

/// Every name claimed by more than one node, with the instances claiming it.
///
/// Surfaced by `peers()` so a collision is visible while it is merely confusing,
/// rather than after it has routed something somewhere wrong.
pub fn name_collisions(reg: &Registry) -> Result<Vec<(String, Vec<String>)>> {
    Ok(name_collisions_of(&all(reg)?))
}

/// The same question asked of rows already in hand.
pub fn name_collisions_of(rows: &[PeerRow]) -> Vec<(String, Vec<String>)> {
    let mut by: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for p in rows {
        by.entry(p.name.clone())
            .or_default()
            .push(p.instance_id.clone());
    }
    by.into_iter().filter(|(_, ids)| ids.len() > 1).collect()
}

/// Drop peers that have been silent well past their TTL.
///
/// The grace multiplier is not decoration: expiring a peer deletes its device
/// rows, which releases their ser2net ports, which renumbers them on return. A
/// blip must cost nothing; a real departure costs a minute.
pub fn expire(
    reg: &mut Registry,
    now: i64,
    ttl_s: u64,
    grace_multiple: i64,
) -> Result<Vec<String>> {
    let cutoff = now - (ttl_s as i64) * 1000 * grace_multiple;
    let doomed: Vec<String> = all(reg)?
        .into_iter()
        .filter(|p| !p.source.never_expires() && p.last_seen < cutoff)
        .map(|p| p.instance_id)
        .collect();
    for id in &doomed {
        reg.conn().execute(
            "DELETE FROM peers WHERE instance_id=?1",
            rusqlite::params![id],
        )?;
    }
    Ok(doomed)
}
