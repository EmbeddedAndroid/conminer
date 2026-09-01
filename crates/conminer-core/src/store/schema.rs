//! Embedded, versioned, forward-only migrations (§14.8).
//!
//! Migrations exist from day one, pre-1.0, because a lab host that has been
//! capturing for a month cannot be asked to throw its history away for a schema
//! change. Each migration is applied inside a transaction and `user_version` is
//! bumped with it, so a crash mid-upgrade leaves the database at the previous
//! version rather than half-migrated.
//!
//! The three migrations mirror the phase plan: the Phase-1 core store, the
//! Phase-4 live/epoch machinery, and the §8.1 search indexes.

/// One forward-only step. `version` is the `user_version` the database has
/// *after* the step is applied.
pub struct Migration {
    pub version: i32,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Device-scoped database (`dev-<canonical>.db`) — one per console.
pub const DEVICE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "core store",
        sql: r#"
-- Device-local metadata: the canonical id this file belongs to, the append-only
-- stream offset, and the retention horizon that makes CURSOR_EXPIRED detectable.
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE sessions (
    id          INTEGER PRIMARY KEY,
    source      TEXT NOT NULL CHECK (source IN ('live','file','pstore','lava')),
    started_at  INTEGER NOT NULL,          -- unix millis, host-stamped UTC
    ended_at    INTEGER,
    label       TEXT,
    -- Content hash of an ingested file, so a duplicate re-ingest is detectable
    -- (§13 `ingest`: idempotent by content hash → new session, warning).
    content_sha TEXT,
    source_path TEXT,
    bytes       INTEGER NOT NULL DEFAULT 0,
    lines       INTEGER NOT NULL DEFAULT 0,
    records     INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX sessions_started ON sessions(started_at);
CREATE INDEX sessions_sha ON sessions(content_sha);

-- Verbatim raw lines. `bytes` is never rewritten, and `offset` is the absolute
-- position in the device's append-only stream — which is what makes two
-- byte-identical boot iterations distinct objects (§8.4).
CREATE TABLE raw_lines (
    id           INTEGER PRIMARY KEY,
    session_id   INTEGER NOT NULL REFERENCES sessions(id),
    boot_id      INTEGER,
    stream_offset INTEGER NOT NULL,
    ts_mono      INTEGER NOT NULL,          -- host monotonic nanos
    ts_wall      INTEGER NOT NULL,          -- unix millis UTC
    stage_id     INTEGER,
    bytes        BLOB NOT NULL,
    terminator   TEXT NOT NULL,
    truncated    INTEGER NOT NULL DEFAULT 0,
    continuation INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE UNIQUE INDEX raw_lines_offset ON raw_lines(stream_offset);
CREATE INDEX raw_lines_session ON raw_lines(session_id, id);
CREATE INDEX raw_lines_boot ON raw_lines(boot_id, id);

CREATE TABLE templates (
    id                 INTEGER PRIMARY KEY,
    stage              TEXT,
    profile            TEXT,
    template_text      TEXT NOT NULL,
    tokens_json        TEXT NOT NULL,
    head_only          INTEGER NOT NULL DEFAULT 0,
    severity           INTEGER NOT NULL DEFAULT 8,   -- most severe record seen
    first_seen_session INTEGER NOT NULL REFERENCES sessions(id),
    first_seen_boot    INTEGER,
    first_seen_ts      INTEGER NOT NULL,
    total_count        INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX templates_first_seen ON templates(first_seen_session);

CREATE TABLE records (
    id            INTEGER PRIMARY KEY,
    session_id    INTEGER NOT NULL REFERENCES sessions(id),
    boot_id       INTEGER,
    first_line_id INTEGER NOT NULL REFERENCES raw_lines(id),
    last_line_id  INTEGER NOT NULL REFERENCES raw_lines(id),
    line_count    INTEGER NOT NULL DEFAULT 1,
    stage_id      INTEGER,
    profile       TEXT NOT NULL,
    severity      INTEGER NOT NULL DEFAULT 8,   -- 0..7 syslog, 8 = unknown
    kind          TEXT NOT NULL DEFAULT 'line', -- line | crash | garbage | binary
    template_id   INTEGER REFERENCES templates(id),
    truncated     INTEGER NOT NULL DEFAULT 0,
    -- Non-destructive extracted fields and framer flags (interleave suspicion,
    -- DEAD_AIR close, retro-attach). The raw line is untouched.
    fields_json   TEXT
) STRICT;
CREATE INDEX records_session ON records(session_id, id);
CREATE INDEX records_template ON records(template_id, id);
CREATE INDEX records_boot ON records(boot_id, id);
CREATE INDEX records_severity ON records(severity, id);

-- Per-session rollup, so "how many times in this run" never scans records.
CREATE TABLE occurrences (
    template_id INTEGER NOT NULL REFERENCES templates(id),
    session_id  INTEGER NOT NULL REFERENCES sessions(id),
    boot_id     INTEGER NOT NULL DEFAULT 0,   -- 0 = not epoch-scoped
    count       INTEGER NOT NULL DEFAULT 0,
    first_ts    INTEGER NOT NULL,
    last_ts     INTEGER NOT NULL,
    PRIMARY KEY (template_id, session_id, boot_id)
) WITHOUT ROWID, STRICT;
CREATE INDEX occurrences_session ON occurrences(session_id);

CREATE TABLE stages (
    id             INTEGER PRIMARY KEY,
    session_id     INTEGER NOT NULL REFERENCES sessions(id),
    boot_id        INTEGER,
    name           TEXT NOT NULL,
    profile        TEXT NOT NULL,
    entered_ts     INTEGER NOT NULL,
    banner_line_id INTEGER REFERENCES raw_lines(id),
    exited_ts      INTEGER
) STRICT;
CREATE INDEX stages_session ON stages(session_id, id);
"#,
    },
    Migration {
        version: 2,
        name: "boot epochs, events, prompts, images",
        sql: r#"
-- §8.4 boot epochs. Every device stream is partitioned into numbered epochs so
-- "did it boot?" can never be answered from the previous boot's output.
CREATE TABLE boots (
    id             INTEGER PRIMARY KEY,
    seq            INTEGER NOT NULL,
    session_id     INTEGER REFERENCES sessions(id),
    label          TEXT,
    opened_by      TEXT NOT NULL CHECK (opened_by IN ('mark','reset','power','flash','session','ingest')),
    opened_at      INTEGER NOT NULL,
    opened_offset  INTEGER NOT NULL,
    closed_at      INTEGER,
    bytes          INTEGER NOT NULL DEFAULT 0,
    -- hash(ordered template-ID sequence ‖ stage timeline) — semantic, so two
    -- byte-different iterations of the same boot still match.
    fingerprint    TEXT,
    outcome        TEXT,
    image_id       INTEGER
) STRICT;
CREATE UNIQUE INDEX boots_seq ON boots(seq);

-- First-class timeline events: line-config changes, power/flash hook
-- invocations, runner transactions, capture-state changes, exclusive claims.
CREATE TABLE events (
    id         INTEGER PRIMARY KEY,
    session_id INTEGER REFERENCES sessions(id),
    boot_id    INTEGER REFERENCES boots(id),
    at            INTEGER NOT NULL,
    stream_offset INTEGER NOT NULL,
    kind          TEXT NOT NULL,
    data_json  TEXT NOT NULL DEFAULT '{}'
) STRICT;
CREATE INDEX events_boot ON events(boot_id, id);
CREATE INDEX events_kind ON events(kind, id);

-- §8.5 prompt expectations, per stage, with provenance.
CREATE TABLE prompts (
    id           INTEGER PRIMARY KEY,
    pattern      TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN
                   ('shell','bootloader','rtos_shell','monitor','credential_gate','ignore')),
    provenance   TEXT NOT NULL CHECK (provenance IN ('profile','configured','learned')),
    stage        TEXT,
    observations INTEGER NOT NULL DEFAULT 0,
    last_seen    INTEGER,
    last_boot_id INTEGER,
    UNIQUE (pattern, stage)
) STRICT;

-- §15.4 build identity bound to epochs, so diffs can be per-build not positional.
CREATE TABLE images (
    id         INTEGER PRIMARY KEY,
    name       TEXT,
    git_sha    TEXT,
    image_hash TEXT,
    source     TEXT NOT NULL CHECK (source IN ('explicit','flash_hook','banner')),
    meta_json  TEXT NOT NULL DEFAULT '{}',
    bound_at   INTEGER NOT NULL,
    UNIQUE (name, git_sha, image_hash)
) STRICT;

-- Derived annotations that must never touch raw (§15.5 symbolization).
CREATE TABLE annotations (
    id        INTEGER PRIMARY KEY,
    record_id INTEGER NOT NULL REFERENCES records(id),
    kind      TEXT NOT NULL,
    data_json TEXT NOT NULL,
    made_at   INTEGER NOT NULL
) STRICT;
CREATE INDEX annotations_record ON annotations(record_id, kind);
"#,
    },
    Migration {
        version: 3,
        name: "FTS5 search indexes",
        sql: r#"
-- §8.1 tier 1. Contentless FTS5 over both raw lines and framed records; record
-- text is its lines joined with \n so a multiline phrase search is the indexed
-- fast path rather than the fallback.
--
-- No stemming: `errno` must not match `error`. Prefix indexes cover partial
-- tokens without dropping to a scan.
CREATE VIRTUAL TABLE raw_fts USING fts5(
    text,
    content='',
    tokenize='unicode61 remove_diacritics 0',
    prefix='2 3 4'
);

CREATE VIRTUAL TABLE record_fts USING fts5(
    text,
    content='',
    tokenize='unicode61 remove_diacritics 0',
    prefix='2 3 4'
);
"#,
    },
    Migration {
        version: 4,
        name: "indexes for open-epoch and open-stage lookup",
        sql: r#"
-- Closing "whatever is currently open" is a per-reset operation, and a board in
-- a boot loop produces thousands of epochs. Without these, each close scans
-- every epoch ever recorded, which turns a 30 MB ingest of a looping board into
-- a quadratic crawl.
CREATE INDEX boots_open ON boots(closed_at) WHERE closed_at IS NULL;
CREATE INDEX stages_open ON stages(session_id) WHERE exited_ts IS NULL;
"#,
    },
    Migration {
        version: 5,
        name: "verdicts, baselines, durable watches",
        sql: r#"
-- Persistent triage. Without this an agent's judgement ("this UFS retry is
-- noise") lives only in its context window, so every fresh session re-reads and
-- re-derives the same table of contents. The verdict is metadata *about* a
-- template; it never touches the template, the records, or the raw bytes.
CREATE TABLE template_verdicts (
    template_id INTEGER PRIMARY KEY REFERENCES templates(id),
    verdict     TEXT NOT NULL CHECK (verdict IN
                  ('benign','known_bad','investigating','interesting')),
    note        TEXT,
    ticket      TEXT,
    author      TEXT,
    updated_at  INTEGER NOT NULL
) STRICT;
CREATE INDEX template_verdicts_verdict ON template_verdicts(verdict);

-- A blessed epoch to diff against. `new_only` is session-scoped, but the
-- question an agent actually asks is "what is new versus the last boot that
-- *worked*", which needs a named, durable reference point.
CREATE TABLE baselines (
    name    TEXT PRIMARY KEY,
    boot_id INTEGER NOT NULL REFERENCES boots(id),
    note    TEXT,
    set_at  INTEGER NOT NULL
) STRICT;

-- Durable watches (§8.2 extension). `follow` is a long poll: if the agent is
-- gone when the predicate fires, the firing is lost. A watch is evaluated
-- against the *stored* stream from its own offset, so an agent that reconnects
-- after twenty minutes still learns what happened and when.
CREATE TABLE watches (
    id             INTEGER PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,
    predicate_json TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    -- How far this watch has been evaluated. Advanced only when its hits are
    -- durably recorded, so a crash mid-scan re-scans rather than skips.
    scanned_to     INTEGER NOT NULL,
    last_polled    INTEGER,
    active         INTEGER NOT NULL DEFAULT 1
) STRICT;

CREATE TABLE watch_hits (
    id            INTEGER PRIMARY KEY,
    watch_id      INTEGER NOT NULL REFERENCES watches(id) ON DELETE CASCADE,
    at            INTEGER NOT NULL,
    stream_offset INTEGER NOT NULL,
    matched       TEXT NOT NULL,
    evidence_json TEXT NOT NULL DEFAULT '{}',
    delivered     INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX watch_hits_pending ON watch_hits(watch_id, delivered, id);
"#,
    },
    Migration {
        version: 6,
        name: "bisect sessions and learned expectations",
        sql: r#"
-- §18.2 bisect. conminer does not flash: it keeps the bookkeeping that makes a
-- bisect correct (which candidate to try next, which verdicts are already in,
-- when the answer is pinned) so the lab's own tooling does the flashing.
CREATE TABLE bisects (
    id              INTEGER PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    -- Ordered candidate list, oldest first: the axis being searched.
    candidates_json TEXT NOT NULL,
    predicate_json  TEXT NOT NULL,
    started_at      INTEGER NOT NULL,
    finished_at     INTEGER,
    culprit         TEXT,
    state           TEXT NOT NULL CHECK (state IN
                      ('running','done','aborted','inconclusive'))
) STRICT;

CREATE TABLE bisect_results (
    bisect_id INTEGER NOT NULL REFERENCES bisects(id) ON DELETE CASCADE,
    idx       INTEGER NOT NULL,
    verdict   TEXT NOT NULL CHECK (verdict IN ('good','bad','skip')),
    boot_id   INTEGER,
    at        INTEGER NOT NULL,
    note      TEXT,
    PRIMARY KEY (bisect_id, idx)
) WITHOUT ROWID, STRICT;

-- §18.4 absence detection. A bring-up failure is usually about what did *not*
-- print, and a novel-template list structurally cannot say that. This is the
-- learned shape of a normal boot on this device: which templates appear, how
-- reliably, and roughly when.
CREATE TABLE expectations (
    template_id     INTEGER NOT NULL PRIMARY KEY REFERENCES templates(id),
    stage           TEXT,
    -- Reference boots that contained it, out of how many were examined.
    seen_in         INTEGER NOT NULL,
    reference_boots INTEGER NOT NULL,
    -- Typical position in the boot, as an ordinal and as ms after epoch open,
    -- so "it printed, but 3 s late" is separable from "it printed".
    median_ordinal  REAL,
    median_offset_ms INTEGER,
    learned_at      INTEGER NOT NULL
) STRICT;
CREATE INDEX expectations_stage ON expectations(stage);
"#,
    },
    Migration {
        version: 7,
        name: "sibling epochs and per-epoch version banners",
        sql: r#"
-- §F1. Actuating a multi-console board opens one epoch per console, and they
-- are the SAME event. Without a shared id an agent that asked the wrong console
-- gets "nothing happened" for a board that booted perfectly on its sibling --
-- measured on the NordAU, where the power epoch landed on a silent console
-- while the evidence accrued on another under a session epoch.
ALTER TABLE boots ADD COLUMN group_id TEXT;
CREATE INDEX boots_group ON boots(group_id) WHERE group_id IS NOT NULL;

-- §F2. What is ACTUALLY RUNNING, lifted at mining time from the version banners
-- each stage prints. The data was always in the store -- BL31's NORDFP stamp,
-- OP-TEE's commit, the UEFI string, the kernel version and #build -- and
-- `provenance.running` returned {} for four rounds because nobody extracted it.
-- Keyed per epoch, because that is the question: what was running THAT boot.
CREATE TABLE epoch_versions (
    boot_id    INTEGER NOT NULL REFERENCES boots(id) ON DELETE CASCADE,
    -- bl2 | bl31 | optee | uefi | xbl | kernel | machine | ...
    component  TEXT NOT NULL,
    version    TEXT NOT NULL,
    -- Whatever else the banner carried: build date, builder, board name.
    detail_json TEXT NOT NULL DEFAULT '{}',
    line_id    INTEGER,
    ts_wall    INTEGER NOT NULL,
    -- An epoch can see the same component twice (kexec, boundary lag). The last
    -- one is what is running; earlier ones are kept and marked.
    superseded INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (boot_id, component, ts_wall)
) WITHOUT ROWID, STRICT;
CREATE INDEX epoch_versions_component ON epoch_versions(component);
"#,
    },
    Migration {
        version: 8,
        name: "pinned metrics",
        sql: r#"
-- §F3. A durable `template_values` query: one number per epoch, named, so
-- "is UEFI init getting slower" is a series instead of three searches and a
-- calculator. The values are not copied -- they are resolved from the template
-- occurrences that already exist -- so pinning costs nothing at capture time
-- and a metric pinned today can answer about boots from last week.
CREATE TABLE metrics (
    name        TEXT PRIMARY KEY,
    template_id INTEGER NOT NULL REFERENCES templates(id),
    -- Which wildcard slot in the template holds the number.
    slot        INTEGER NOT NULL DEFAULT 0,
    -- last | first | min | max, when an epoch contains several occurrences.
    agg         TEXT NOT NULL DEFAULT 'last'
                  CHECK (agg IN ('last','first','min','max')),
    unit        TEXT,
    pinned_at   INTEGER NOT NULL
) WITHOUT ROWID, STRICT;
"#,
    },
    Migration {
        version: 9,
        name: "watch push delivery",
        sql: r#"
-- §K4. A watch that can PUSH. `follow` covers a parked agent; an unattended
-- overnight soak has no agent at all, and until someone polls, a board that
-- started flapping at 2am is a fact nobody holds.
--
-- Delivery is best-effort NOTIFICATION and never authoritative: `poll_watch`
-- remains the source of truth, and a firing is never consumed by a delivery
-- that was not acknowledged. The counters exist so "the webhook is silently
-- failing" is a number rather than a suspicion.
ALTER TABLE watches ADD COLUMN notify_url TEXT;
ALTER TABLE watches ADD COLUMN notify_secret TEXT;
ALTER TABLE watches ADD COLUMN notify_min_interval_s INTEGER NOT NULL DEFAULT 60;
ALTER TABLE watches ADD COLUMN delivered INTEGER NOT NULL DEFAULT 0;
ALTER TABLE watches ADD COLUMN delivery_failed INTEGER NOT NULL DEFAULT 0;
ALTER TABLE watches ADD COLUMN last_status INTEGER;
ALTER TABLE watches ADD COLUMN last_delivery_at INTEGER;
-- The high-water mark of what has been ACKNOWLEDGED, distinct from
-- `scanned_to`: an undelivered firing must stay deliverable.
ALTER TABLE watches ADD COLUMN delivered_to INTEGER NOT NULL DEFAULT 0;
"#,
    },
    Migration {
        version: 10,
        name: "watch delivery backoff is scheduled, not slept",
        sql: r#"
-- §L1. The retry backoff used to be three sleeps INSIDE the sweep (5 s, 25 s,
-- 125 s). One unreachable receiver therefore held the whole sweep for up to
-- two and a half minutes, and every OTHER board's watch waited behind it --
-- measured on this rig, where a live watch's first post landed 32 s late
-- because a dead endpoint was still being retried ahead of it.
--
-- A streak makes the backoff a SCHEDULE: the sweep posts once, records the
-- outcome, and the next attempt is simply not due yet. Nothing sleeps, so a
-- dead receiver costs its own watch and nobody else's.
ALTER TABLE watches ADD COLUMN delivery_fail_streak INTEGER NOT NULL DEFAULT 0;
"#,
    },
];

/// Global registry database — device identity, nicknames, tags, port
/// assignments, leases and targets. Small, and the only cross-device state.
pub const REGISTRY_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "registry",
        sql: r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE devices (
    id            INTEGER PRIMARY KEY,
    -- Canonical id: the /dev/serial/by-id path. Survives replug, host reboot and
    -- ttyUSBn renumbering.
    canonical     TEXT NOT NULL UNIQUE,
    -- Positional fallback for serial-less clone adapters: /dev/serial/by-path.
    by_path       TEXT,
    identity_kind TEXT NOT NULL CHECK (identity_kind IN ('by_id','positional')),
    -- Informational only; never appears in tool responses except in `identify`.
    tty           TEXT,
    nickname      TEXT UNIQUE,
    pinned_profile TEXT,
    ser2net_port  INTEGER UNIQUE,
    line_json     TEXT NOT NULL DEFAULT '{}',
    target        TEXT,
    state         TEXT NOT NULL DEFAULT 'unknown',
    ignored       INTEGER NOT NULL DEFAULT 0,
    first_seen    INTEGER NOT NULL,
    last_seen     INTEGER NOT NULL,
    -- §3.1 observed identity: what the console has shown it is.
    observed_json TEXT NOT NULL DEFAULT '{}',
    db_file       TEXT NOT NULL
) STRICT;
CREATE INDEX devices_target ON devices(target);

CREATE TABLE tags (
    device_id INTEGER NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    key       TEXT NOT NULL,
    value     TEXT NOT NULL,
    PRIMARY KEY (device_id, key)
) WITHOUT ROWID, STRICT;
CREATE INDEX tags_kv ON tags(key, value);

-- §15.1 device reservation. Reads are always unrestricted; mutating tools need
-- the lease.
CREATE TABLE leases (
    device_id   INTEGER PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    holder      TEXT NOT NULL,
    token       TEXT NOT NULL,
    acquired_at INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    stolen_from TEXT
) STRICT;

-- §15.2 binary-protocol claim: capture continues, interpretation suspends.
CREATE TABLE exclusive_claims (
    device_id  INTEGER PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    holder     TEXT NOT NULL,
    protocol   TEXT,
    claimed_at INTEGER NOT NULL
) STRICT;
"#,
    },
    Migration {
        version: 2,
        name: "fleet peering",
        sql: r#"
-- §P1. THE FLEET TABLE. Soft state, TTL'd, and deliberately in the registry
-- rather than in memory: mcpd and dashd are separate processes and both need to
-- know who the peers are without asking peerd over yet another socket.
CREATE TABLE peers (
    instance_id   TEXT PRIMARY KEY,     -- uuid4, stable across renames
    name          TEXT NOT NULL,        -- what a human types: "alpha"
    host          TEXT,                 -- address other nodes reach it on
    mcp_url       TEXT NOT NULL,
    dash_url      TEXT,
    ser2net_host  TEXT,
    version       TEXT,
    source        TEXT NOT NULL,        -- 'beacon' | 'mdns' | 'static'
    ok            INTEGER NOT NULL DEFAULT 1,
    last_seen     INTEGER NOT NULL,
    last_error    TEXT,
    advert_count  INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX peers_name ON peers(name);

-- Which node owns a device. NULL means this one -- so every existing row keeps
-- its meaning without a backfill, and "local" stays the cheap default path.
--
-- `node` is the peer NAME (what an operator types and what the dashboard
-- shows); `node_host` is its address, denormalised onto the row so a device
-- listing can say "adp-ventuno on alpha (192.168.10.10)" without a join in
-- every reader. `remote_canonical` is the id the OWNER knows it by, which is
-- what a proxied call must carry.
ALTER TABLE devices ADD COLUMN node TEXT;
ALTER TABLE devices ADD COLUMN node_host TEXT;
ALTER TABLE devices ADD COLUMN remote_canonical TEXT;
-- The port the OWNER serves this console on. Ours re-exports to it.
ALTER TABLE devices ADD COLUMN remote_port INTEGER;
CREATE INDEX devices_node ON devices(node);
"#,
    },
    Migration {
        version: 3,
        name: "multi-hop peer routing",
        sql: r#"
-- §P2. WHO OWNS IT versus HOW TO GET THERE.
--
-- `node` has always named the OWNER, and until now that was also the next hop:
-- every remote row was learned from the node that owns it, one hop away. With
-- transitive routing they come apart -- a node can learn about a board through a
-- peer that merely relays to its owner -- and conflating them is how a call gets
-- addressed to a machine that cannot answer for it.
--
-- `via` is the peer to FORWARD to. NULL means the owner is directly peered, which
-- keeps every existing row correct with no backfill and keeps the one-hop case on
-- the cheap path.
ALTER TABLE devices ADD COLUMN via TEXT;
-- How far away the OWNER is. 1 = directly peered. Bounded on import, so a cycle
-- between three nodes cannot materialise devices for ever -- the hazard that made
-- the first version refuse two hops outright.
ALTER TABLE devices ADD COLUMN hops INTEGER NOT NULL DEFAULT 1;
"#,
    },
    Migration {
        version: 4,
        name: "remote controls",
        sql: r#"
-- §P2. WHAT THE OWNER SAYS ABOUT ITS OWN BOARD.
--
-- A remote row's controller cannot be resolved here: the profiles match on a
-- by-id name, and the controller they look for is plugged into ANOTHER host. So
-- a peer's board showed no controller and no power buttons -- the hardware is
-- driveable, and the only node that could say so was not asked.
--
-- Stored as the owner's own answer (controller name, offerable boot modes,
-- whether a power hook resolved there) rather than recomputed, because the owner
-- is the only node that can see its own controllers.
ALTER TABLE devices ADD COLUMN remote_controls TEXT;
"#,
    },
    Migration {
        version: 5,
        name: "reverse-channel poll time",
        sql: r#"
-- §P3. WHEN THIS PEER LAST ASKED US FOR WORK.
--
-- `ok` answers "can we reach it"; this answers "can it reach us", and on a
-- one-way link those differ. It lives in the table rather than in the mcpd
-- process because dashd is a separate process and would otherwise render a node
-- that is fully driveable -- calling in every twenty seconds -- as a host whose
-- mcpd has crashed. An amber lamp on a working bench is worse than no lamp.
ALTER TABLE peers ADD COLUMN last_poll INTEGER;
"#,
    },
    Migration {
        version: 6,
        name: "capture health is not presence",
        sql: r#"
-- TWO FACTS, TWO COLUMNS. `state` carried both "is this device plugged in"
-- (written by discoveryd: discovered / gone / ignored) and "is capture working"
-- (written by minerd: listening / streaming / garbage / open_failed /
-- away_in_edl / not_listening). Two processes, one column, last writer wins --
-- so a presence sweep erased capture truth and a capture update erased
-- presence. Reported from the bench as `capture_state: not_listening` on a
-- console that was capturing a boot at the time.
--
-- `state` stays presence, which is what discovery owns and what every existing
-- reader of "gone" means. Capture health moves here. Backfilled from whatever
-- `state` happens to hold, so a device that was mid-capture at upgrade keeps
-- its answer instead of reading as unknown.
ALTER TABLE devices ADD COLUMN capture_state TEXT;
UPDATE devices SET capture_state = state
 WHERE state IN ('listening','streaming','garbage','open_failed','away_in_edl','not_listening');
"#,
    },
    Migration {
        version: 7,
        name: "give presence its column back",
        sql: r#"
-- Migration 6 moved capture health to its own column but left the old value
-- standing in `state`, so presence stayed frozen at whatever minerd wrote last.
-- Measured on both live nodes: EVERY row read `state: not_listening`, including
-- consoles that were capturing at the time. Discovery only writes on a
-- transition, so nothing ever corrected it.
--
-- Restore a presence value. Discovery owns this column and will replace these on
-- its next sweep; the point is to stop presence claiming to be health in the
-- meantime.
UPDATE devices SET state = CASE
    WHEN ignored = 1            THEN 'ignored'
    WHEN ser2net_port IS NOT NULL THEN 'discovered'
    ELSE 'unknown' END
 WHERE state IN ('listening','streaming','garbage','open_failed','away_in_edl','not_listening');
"#,
    },
    Migration {
        version: 8,
        name: "agent bug reports",
        sql: r#"
-- §R1. WHAT AN AGENT FOUND, WITH THE EVIDENCE ALREADY ATTACHED.
--
-- Reports used to travel as prose pasted between a human and a fixer, and the
-- first job on every one was reconstructing which board, which epoch and which
-- build it was about. All of that is already in this process when the agent
-- notices, so it is recorded here instead of remembered.
--
-- In the REGISTRY, not a device store: half of what agents report is about the
-- tool surface (a verdict, a predicate, an envelope) and names no device at all.
CREATE TABLE reports (
    id           INTEGER PRIMARY KEY,
    -- The dedupe key. Same trick the framer uses on console lines: reports
    -- arriving in different words about the same thing collapse into one row
    -- with a count, so triage sees eight problems rather than forty messages.
    fingerprint  TEXT NOT NULL UNIQUE,
    title        TEXT NOT NULL,
    -- What the agent expected versus what it got. Two fields rather than one
    -- blob, because that pair is what turns a report into a test.
    expected     TEXT,
    observed     TEXT,
    -- Where it happened. All optional: a report about a tool surface has no
    -- device, and one filed from a fresh session has no epoch.
    device       TEXT,
    boot_id      INTEGER,
    cursor       TEXT,
    tool         TEXT,
    args_json    TEXT,
    -- Which code saw it. `build` is the fingerprint the node was running, and
    -- it is what makes a resolution checkable rather than a claim.
    build        TEXT,
    node         TEXT,
    reporter     TEXT,
    status       TEXT NOT NULL DEFAULT 'open'
                 CHECK (status IN ('open','fixed','not_a_bug','wont_fix','duplicate')),
    -- Set when resolved. A repeat arriving from a build at or after this one is
    -- a REGRESSION, not a duplicate -- which is exactly what nothing noticed
    -- when a fixed EDL verdict came back.
    fixed_in_build TEXT,
    -- The gate that holds the fix. A resolution without one is a claim; with
    -- one it is checkable.
    gate         TEXT,
    resolution   TEXT,
    occurrences  INTEGER NOT NULL DEFAULT 1,
    regressions  INTEGER NOT NULL DEFAULT 0,
    first_seen   INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL,
    resolved_at  INTEGER
) STRICT;
CREATE INDEX reports_status ON reports(status, last_seen);

-- §R2. WHO ELSE HIT IT. One row per sighting, so "me too" is evidence rather
-- than a bumped counter: three agents on three nodes is a different priority
-- from one agent retrying, and only the per-sighting build makes a regression
-- distinguishable from a duplicate.
CREATE TABLE report_seen (
    id         INTEGER PRIMARY KEY,
    report_id  INTEGER NOT NULL REFERENCES reports(id) ON DELETE CASCADE,
    reporter   TEXT,
    node       TEXT,
    build      TEXT,
    device     TEXT,
    boot_id    INTEGER,
    cursor     TEXT,
    note       TEXT,
    at         INTEGER NOT NULL
) STRICT;
CREATE INDEX report_seen_report ON report_seen(report_id, at);
"#,
    },
];
