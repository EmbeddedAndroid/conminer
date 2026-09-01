# conminer — Serial Console Template Miner for LLM Agents

**Status:** Draft v0.2 · **License:** Apache 2.0 · **Deployment:** Alpine containers via docker-compose

**Project rule:** every feature and every enumerated edge case ships with a test suite. No phase is complete until its test gate (§12.6) passes. This is intended to be the most reliable piece of infrastructure in the lab.

A Drain-style streaming template miner and query service for serial console logs, designed so any MCP-capable agent can debug embedded targets without ever paging raw multi-megabyte logs through its context window. The agent sees a *table of contents* (deduplicated templates with counts and timelines) and drills into verbatim raw lines only on demand.

---

## 1. Upstream analysis and positioning

**uart-mcp (AdolphNB/uart-mcp, MIT)** was evaluated as the upstream integration target. Findings:

- ~10-file single-maintainer Python project (3 stars, 21 commits, no releases)
- Single serial port at a time; logs held in a **1,000-line in-memory ring buffer** — no persistence, no structure, no scale path to 800 MB
- No plugin or extension framework; the documented extension model is "add a `@mcp.tool()` function to `mcp_server.py`"
- Tool surface: `get_serial_status`, `query_serial_logs` (regex over buffer), `get_recent_logs`, `clear_log_buffer`, `send_serial_command`, `get_log_buffer_info`

**Conclusion:** there is no pluggable framework upstream to hook into. This justifies a standalone project. We stay *upstream-friendly* three ways:

1. **Permissive license** and small, importable core library, so uart-mcp (or anyone) can vendor the miner.
2. **Compatible semantics:** our raw-access tools are a superset of uart-mcp's (`search_raw` ≈ `query_serial_logs`, `get_recent` ≈ `get_recent_logs`), so agents/prompts written for uart-mcp port trivially.
3. **ser2net as the sharing layer:** we never claim exclusive ownership of `/dev/tty*`. ser2net fans each physical port out over TCP/RFC2217, so uart-mcp, minicom, labgrid, and conminer can all attach to the same console simultaneously. uart-mcp remains fully usable alongside us for TX and interactive work.

Longer-term upstream paths (Phase 6): propose a `get_log_templates` tool to uart-mcp backed by our library; propose a labgrid exporter integration so LAVA/labgrid labs get mining for free.

## 2. Language decision

**Recommendation: Rust.**

| Criterion | Rust | Mojo | Python |
|---|---|---|---|
| MCP SDK | Official `rmcp` SDK (stdio + streamable HTTP) | None | Official SDK, mature |
| Serial/TCP IO | `tokio`, `tokio-serial`, `serialport` — mature | Immature ecosystem | `pyserial` — mature |
| 800 MB post-hoc mining | Streams at disk speed, low RSS | Unproven | Workable but slow (Drain3 is pure Python) |
| Alpine image | Static musl binary; final image ≈ 15 MB `FROM alpine` | No musl static story | ~120 MB+ with interpreter |
| Drain implementation | `drain-rs` crate exists to evaluate; algorithm is ~300 LoC to own | Would write from scratch | Reference `drain3` library |

Mojo is not viable today: no MCP SDK, no serial ecosystem, immature async story, and no clean static-binary path for Alpine. Rust gives us the static-musl container story the docker-compose design wants, and the Drain algorithm is small enough that owning a from-scratch implementation (with `drain-rs` as a reference) is low-risk and removes the last dependency question.

Fallback: if Phase 1 velocity matters more than the deployment story, the core library could be prototyped in Python with `drain3` and ported. The spec below is language-neutral except where noted.

## 3. System architecture

Four services under docker-compose, one shared volume for state:

```
                         ┌───────────────────────────────────────────┐
 /dev/serial/by-id/* ───▶│ discoveryd                                │
   (udev hotplug)        │  scans + watches serial devices,          │
                         │  writes registry, regenerates ser2net.yml │
                         └───────────────┬───────────────────────────┘
                                         │ registry.json + SIGHUP
                                         ▼
                         ┌───────────────────────────────────────────┐
                         │ ser2net                                   │
   humans, uart-mcp, ───▶│  one TCP/RFC2217 endpoint per device      │◀── labgrid,
   minicom, agents (TX)  │  (ports 5001..500N), multi-consumer       │    CI, etc.
                         └───────────────┬───────────────────────────┘
                                         │ TCP (read side)
                                         ▼
                         ┌───────────────────────────────────────────┐
                         │ minerd                                    │
                         │  per-device pipeline:                     │
                         │  bytes → lines → FRAMER (records) →       │
                         │  DRAIN (templates) → SQLite (WAL)         │
                         │  + file-ingest workers for post-hoc jobs  │
                         └───────────────┬───────────────────────────┘
                                         │ query API (unix socket)
                                         ▼
                         ┌───────────────────────────────────────────┐
                         │ mcpd                                      │
                         │  MCP server: streamable HTTP (network)    │
                         │  and stdio (docker exec) transports       │
                         └───────────────────────────────────────────┘
```

- **discoveryd** — watches `/dev/serial/by-id/` (udev netlink inside the container via mounted `/dev` + `/run/udev`), maintains `registry.json` (stable device identity = by-id path), templates a `ser2net.yaml`, and signals ser2net to reload. A plugged-in FTDI cable is queryable by agents within ~1 s with zero configuration.
- **ser2net** — stock upstream ser2net (Alpine package), config fully generated. Gives every device a stable TCP endpoint and lets N consumers share the console. This is the composition point with uart-mcp and the existing HIL stack.
- **minerd** — the core. One async task per device consuming its ser2net endpoint, plus a job queue for post-hoc file ingestion. Both paths feed the identical framer→drain→store pipeline, satisfying the "live and post-hoc, agent-configurable" requirement.
- **mcpd** — thin MCP layer over minerd's query API. Streamable HTTP so remote agents (CI, Claude Code on a laptop, the HIL agent) connect over the network; stdio mode for local single-agent use.

All four images are Alpine-based; Rust services build with `cargo build --release --target x86_64-unknown-linux-musl` (and aarch64 for lab hosts on Arm) in a multi-stage Dockerfile.

### 3.1 Device identity & addressing at scale

Plugging in N consoles must never create ambiguity about which one an agent is talking to. Identity has four layers, and every tool's `device` parameter is a **selector** resolved against all of them:

1. **Canonical ID** — the stable identity discoveryd keys on: the `/dev/serial/by-id/` path (adapter USB serial number). Survives replug, host reboot, and ttyUSBn renumbering. **By-id is used everywhere internally**: generated ser2net configs open the by-id symlink (never `/dev/ttyUSBn`), the store and all device references persist the by-id path, and no ttyUSBn name ever appears in tool responses except as informational detail in `identify`. For serial-less clone adapters (the FTDI-clone problem, already in the `discovery` test suite), identity falls back to **USB topology position** via `/dev/serial/by-path/` (`bus-port.port.port`) — "the cable in that physical port" — with the caveat surfaced in `list_devices` (`identity: positional`) so humans know moving the cable moves the name.
2. **Nickname** — human/agent-assigned via `name_device(selector, nickname)`: `rb3-ap`, `bench-left`, `soakrig-04`. Bound to the canonical ID, persisted in the registry, unique-enforced.
3. **Tags** — arbitrary key/values via `tag_device(selector, {soc: qcs6490, rack: r2, role: ap-console, owner: ops})`. `list_devices(filter)` and selectors both accept tag queries, which is how fleets stay navigable at 30+ consoles.
4. **Observed identity** — what the console has *shown* it is: discoveryd keeps the latest BANNER_VERSION extractions per device (last U-Boot build tag, kernel version, Zephyr board name, EC image string). Free target fingerprinting: `list_devices` shows not just "FTDI on port 7" but "last booted: qcm6490 / U-Boot 2026.01 / Linux 6.12.9".

**Selector resolution**, in order: exact nickname → exact canonical ID → `tag:` query (`tag:role=ap-console AND tag:rack=r2`) → unique unambiguous substring of any of the above. Anything that resolves to ≠1 device returns structured `AMBIGUOUS_DEVICE` (with the candidate list and their observed identities) or `UNKNOWN_DEVICE` — a tool call never guesses, and the candidate list usually lets the agent disambiguate in one step without a human. Group selectors (`tag:` matching many) are valid only for explicitly multi-device tools (`list_devices`, target-scoped queries per §15.8), never for `run_command`/`power`/`flash`.

**`identify(selector)`** helps humans map cable↔board without risk: returns the device's observed identity, last-traffic snippet, USB topology, and ser2net endpoint — read-only by default; an optional `dtr_pulse: true` mode is available but flagged loudly (DTR is wired to reset on many boards) and requires the §15.1 lease.

**Tests (extends `discovery` suite):** nickname survives replug and renumbering; positional identity follows the port not the cable, and says so; two clones in adjacent ports stay distinct; selector precedence (nickname shadowing a substring match); ambiguous substring → `AMBIGUOUS_DEVICE` with candidates; tag-query resolution incl. multi-tag AND; group selector rejected on single-device tools; nickname uniqueness collision → structured error; observed identity updates on new banner and survives restart; `identify` without lease is read-only, `dtr_pulse` without lease refused.

### 3.2 UART line settings

**Default: `115200 8N1, flow=none`** — applied to every newly discovered device with no other source of truth. Settings are layered, most-specific wins:

1. built-in default (`115200 8N1 none`)
2. global config (`conminer.toml [line_defaults]`)
3. per-device registry entry — set via config file or persisted `set_line`
4. per-stage override (rare but real: a bootloader console at 115200 with an application console reconfigured to 921600; stage transitions apply the override automatically and record it)
5. runtime `set_line(device, {baud, data_bits, parity, stop_bits, flow}, persist?)` — lease-gated (§15.1); `persist: true` updates the registry, otherwise reverts on next session

**Ownership and visibility:** conminer's registry is the source of truth; ser2net configs are generated from it. RFC2217 technically lets any attached consumer renegotiate the line — that affects *all* consumers of the port, so the documented contract is: change settings through `set_line`, and minerd treats an unexplained transition to GARBAGE_BURST as a possible out-of-band line change (the auto-baud prober of §15.9, when enabled, then re-locks and reports what it found). Every settings change is recorded as a **line-config event** in the device's stream timeline — so garbage before/after a change is attributable to it, and `boot_report`/`get_context` can show "settings changed here." Current settings appear in `list_devices`, `identify`, and the freshness envelope.

**Non-standard defaults by ecosystem** are just per-device or profile-suggested registry entries (e.g. 1500000 for some SoCs' debug UARTs, 921600 for high-rate trace consoles, 9600 for legacy/BMC gear) — profiles may *suggest* a rate on banner detection but never apply one without §15.9 auto-baud being enabled.

**Tests (suite `line`, added to §13):** default applied on first discovery; precedence order incl. per-stage override firing on stage transition and reverting; `set_line` without lease refused; `persist` vs ephemeral behavior across session end; line-config event lands in the timeline at the right offset; generated ser2net config reflects registry settings after change + reload; out-of-band RFC2217 change by a foreign consumer → GARBAGE_BURST → (with auto-baud on) re-lock + report, (off) `garbage` state with line-change hypothesis noted; 7E1/odd-parity device end-to-end through a pty honoring the settings.

## 4. Ingestion modes

**Live:** automatic for every discovered device. Ring-buffer of raw bytes (default 64 MB/device on disk, not RAM) plus full structured capture in SQLite. Sessions are opened on first byte after silence/boot-marker and on explicit `start_session` calls.

**Post-hoc:** `ingest_file` MCP tool (and `conminer ingest` CLI) accepts a path or uploaded artifact (LAVA job logs, field captures, `dmesg` dumps). 10 KB–800 MB per the requirement; an 800 MB file is a streaming single pass — target ≥ 100 MB/s on lab-host hardware, so worst case ≈ 8 s. Ingestion creates a session tagged `source=file` and is queryable through the exact same tools as live data.

## 5. Record framing (multi-line grouping) — pluggable profiles

The framer turns a line stream into *records*: a record is one logical event (a single printk, or an entire 80-line kernel oops, U-Boot exception dump, Zephyr fatal-error backtrace, TF-A panic). Framing runs **before** Drain so a looping panic dedupes as one template, not eighty.

**Profile model:** a framer profile is (a) line-shape matchers, (b) record start/continuation/terminator rules, and (c) non-destructive field extractors (timestamp, severity, cpu, module). Two tiers:

- **Declarative profiles** (TOML + regex/state-machine rules) — covers most firmware; users drop a file in `profiles.d/` without recompiling.
- **Native plugins** (Rust trait `Framer`, loaded statically or via a small dynamic registry) — for stateful cases like ANSI-heavy UEFI output or interleaved SMP oops reassembly.

**Built-in profiles (initial set):**

| Profile | Handles |
|---|---|
| `linux` | printk timestamps, loglevels, multi-line oops/panic/BUG/WARN with `Call Trace:`/`Backtrace:` continuation, lockdep splats, OOM reports |
| `uboot` | SPL/U-Boot banners, environment prints, exception/data-abort dumps, autoboot countdown |
| `uefi` | EDK2 DEBUG output, ANSI stripping, ASSERT/exception context dumps |
| `tfa` | TF-A (BL1/BL2/BL31) notice/error format, panic + register dumps |
| `optee` | OP-TEE core traces (`E/TC`, `I/TC`), TA panics with call stacks |
| `zephyr` | Zephyr log backend formats, `<err>`/`<wrn>` levels, fatal error + esf register dumps |
| `freertos` | No standard format — heuristic profile + assert/stack-overflow hook patterns; expected to be customized per project |
| `threadx` | Same approach as freertos |
| `raw` | Fallback: every line is a record |

**Prior art to seed the `linux` profile:** LAVA's dispatcher ships kernel-message detection (`lava_dispatcher/utils/messages.py`, `LinuxKernelMessages`) — pexpect patterns for panic/oops/BUG/trace that LAVA inserts into its boot pipelines; its known limitation is that it only watches the boot portion of a job. Linaro's SQUAD `linux_log_parser.py` plugin extends the same regex family across full job logs and attaches log snippets to results. Both are GPL-2.0+ Python, so we don't vendor code — we port the *pattern sets* (with attribution) as the severity/record-start seed rules for the `linux` framer profile, and validate our framing against LAVA job logs in the corpus (§12.2). This also gives the LAVA triage path a consistency check: conminer and LAVA should agree on what counts as a panic.

**Boot-stage tracking:** embedded consoles change dialect mid-stream (BootROM → TF-A → U-Boot → kernel → userspace, or MCUboot → Zephyr). The framer layer runs a stage state machine keyed on banner signatures; the active profile switches automatically and every record is tagged with its stage. Stage transitions are first-class events (`boot_stages` tool) — "did it die in BL31 or after handoff?" becomes a one-call answer. Auto-detection can be pinned per device (`profile=zephyr`) when heuristics shouldn't run.

## 6. Template mining without masking

Constraint: **no masking of any kind.** Design consequences:

- **Raw lines are stored verbatim, always.** Nothing in the pipeline rewrites, redacts, or normalizes captured bytes. Field extraction in the framer is non-destructive (extracted fields are stored *alongside* the untouched raw line).
- Drain's masking step is a *preprocessing accuracy aid*, not part of the core algorithm — the algorithm itself clusters by token count + positional similarity and produces `<*>` wildcards wherever tokens disagree. We run **pure Drain**: fixed-depth prefix tree, similarity threshold (default 0.4, per-profile tunable), max-children with `<*>` overflow node.
- Templates are therefore *derived views* over stored raw records; deleting the template store loses nothing but an index, and `rebuild_templates` can regenerate it from raw at any time (also how threshold tuning is done retroactively).
- Known cost of no masking: messages whose variable part changes token *count* (not just value) fragment into sibling templates, and high-cardinality hex tokens rely on wildcard convergence rather than a `<HEX>` mask. Mitigations that stay within the constraint: per-profile tokenizer rules (e.g. treat `=`-joined `key=value` as two tokens) and template *merge suggestions* surfaced as data (never auto-applied to raw).
- Framed multi-line records are templated on a normalized key line (first line of the record) with the record body attached; bodies are additionally sub-templated line-by-line so two oopses with different call stacks are distinguishable via `template_detail`.

## 7. Persistence and data model

SQLite (WAL mode), one database file per device on the shared volume, plus a small global registry DB. Rationale: zero-ops, fits Alpine, handles 800 MB ingests fine with WAL + batched transactions, trivially backed up by copying the volume.

```
devices(id, by_id_path, nickname, pinned_profile, first_seen)
sessions(id, device_id, source{live,file}, started_at, ended_at, label)
raw_lines(id, session_id, ts_mono, ts_wall, stage_id, bytes BLOB)        -- verbatim
records(id, session_id, first_line_id, last_line_id, stage_id,
        profile, severity, template_id)
templates(id, device_id, stage, profile, template_text,
          first_seen_session, first_seen_ts, tokens_json)
occurrences(template_id, session_id, count, first_ts, last_ts)           -- rollup
stages(id, session_id, name, entered_ts, banner_line_id)
```

Cross-session template identity is per **device** (template IDs stable across reboots/sessions), which makes the two killer queries cheap:

- *"What's new?"* — templates whose `first_seen_session` = current session.
- *"What changed between run A and run B?"* — anti-join on `occurrences`.

Retention: raw lines pruned per-device by size/age policy (default keep-all for file sessions, 2 GB cap for live); templates and occurrence rollups are kept forever (they're tiny).

## 8. MCP tool surface

Read side (mcpd):

| Tool | Purpose |
|---|---|
| `list_devices()` | Discovered devices, endpoints, active profile/stage, session state |
| `list_sessions(device, limit)` | Session history incl. file ingests |
| `start_session(device, label)` / `end_session` | Explicit session boundaries for test runs |
| `ingest_file(path, device?, profile?)` | Post-hoc mining; returns session_id |
| `list_templates(session|device, stage?, min_count?, severity>=?, new_only?, order)` | The table of contents. `new_only=true` = first seen this session |
| `template_detail(template_id, examples=3)` | Full template, counts, first/last seen, timeline buckets, N verbatim example records |
| `get_records(template_id, session, n, offset)` | Verbatim raw records for a template |
| `get_context(line_id, before, after)` | ±N verbatim raw lines around any line/record |
| `search_raw(pattern, session, max_results)` | Regex over raw store (uart-mcp `query_serial_logs` parity) |
| `get_recent(device, lines)` | Tail of live console (uart-mcp `get_recent_logs` parity) |
| `diff_sessions(a, b)` | Templates new in b / gone from b / count-shifted |
| `boot_stages(session)` | Stage timeline with banner line refs |
| `stats(session)` | Line/record/template counts, compression ratio, ingest rate |

Notifications: mcpd emits MCP resource-updated notifications on **novel template** and **stage transition** events so a supervising agent (e.g. the HIL agent watching a soak test) reacts without polling.

### 8.2 Incremental consumption — how agents follow a live boot

Agents never poll dumb loops and never receive pushes they didn't ask for. The model is **cursor + server-side predicate long-poll**:

- Every read tool returns an opaque, monotonic **cursor** (position in the device's line stream). Cursors survive reconnects and are valid until retention prunes past them (then a structured `CURSOR_EXPIRED` error tells the agent to re-anchor).
- **`follow(device, cursor, until, timeout, max_lines)`** — the tailing primitive. Blocks server-side until `until` is satisfied or `timeout` expires, then returns *everything since the cursor* in mined form — new records, new/incremented templates, stage transitions — plus the raw tail up to `max_lines` and the next cursor. `until` is a predicate, any of:
  - `pattern: <regex>` — a specific line appears
  - `template: new` — any novel template (crash you haven't seen)
  - `stage: <name>` — boot reaches a stage (e.g. `userspace`)
  - `prompt: true` — the device's prompt is detected (see §8.3) — this is the "boot finished" signal
  - `quiet: <ms>` — console goes silent for a duration (settle detection)
  - `any: [...]` — first of several
- So "boot the board and tell me when it's up or crashed" is one call: `follow(dev, cursor, until={any:[{prompt:true},{template:new},{quiet:30000}]}, timeout=120s)` — the agent burns zero tokens during the 90 boring seconds of boot, and the response says *which* predicate fired. Repeated `follow` calls with returned cursors give gap-free incremental consumption; templates make each increment cheap (a boot loop iterating 40 times between calls returns count deltas, not 40× raw output).
- Timeouts return `matched: none` with the data so far — a timeout is data, not an error.
- MCP notifications (novel template, stage transition) remain for supervisory agents, but `follow` is the primary mechanism: it works over plain request/response and composes with agent tool-calling loops naturally.

### 8.3 Interactive command runner — reliable UART transactions

Post-boot interaction is in scope (supersedes v0.1's TX-out-of-scope stance). Raw `send` over a shared serial line is unreliable by construction — dropped characters, echo interleave, multi-writer collisions, hung foreground processes. The runner makes a command a **transaction** with the discipline of a purpose-built serial runner:

**Prompt registry.** Per-device ordered prompt set: profile-supplied defaults (`=> ` U-Boot, `uart:~$ ` Zephyr shell, `# `/`$ ` Linux, `grub> `, EC `> `…) plus device config and a `learn_prompt(device)` helper that observes the idle line. Prompt regexes are validated for distinctiveness (LAVA's lesson: a prompt like `:` matches status output; reject/warn on non-distinctive prompts). The active prompt also drives `follow`'s `prompt:true` predicate.

**`run_command(device, cmd, opts)` transaction lifecycle:**
1. **Acquire** the per-device TX lock (all interactive traffic serializes through the runner — two agents can both call it, transactions queue; direct ser2net TX remains possible but is documented as forfeiting reliability guarantees).
2. **Assert idle:** verify the prompt is present (send bare `\n` probe if the line has been quiet and no prompt is buffered; `quiet` predicate confirms settle). If no prompt → structured `NO_PROMPT` error with the tail attached — never fire a command into an unknown state.
3. **Send character-at-a-time** with configurable inter-character delay (default 10 ms; per-device override for slow bootloader consoles) and **per-character echo verification**: after each char, wait for its echo (timeout per char); on mismatch/timeout, retry the char once, then abort the transaction with `ECHO_MISMATCH` and the observed bytes. Echo verification is skippable per command (`echo: off`) for no-echo consoles, which downgrades to pacing-only mode.
4. **Terminate** with the line ending from device config (`\n`, `\r\n`, or `\r`).
5. **Capture** all output until the prompt returns, tagging every line with the transaction ID (output is simultaneously mined — a command that triggers an oops produces both the command's output record *and* the crash record, linked).
6. **Return** `{status: ok, output (capped + record IDs), duration, prompt_matched}`.

**Hung-command recovery ladder** (when the prompt does not return within `timeout`): each rung is attempted in order, logged as part of the transaction, and stops at the first rung that restores the prompt:
1. bare `\n` probe (prompt merely scrolled away?)
2. `Ctrl-C` (SIGINT the foreground process), re-probe
3. `Ctrl-\` then `Ctrl-D` (configurable escape set; some consoles need `~.` or `Ctrl-]`)
4. declare **hung**: release the lock, mark device health `hung`, emit a notification, return `{status: hung, rungs_attempted, output_so_far}` — and optionally invoke the device's configured **external reset hook** (shell command / HTTP call, e.g. a labgrid `power cycle` or PDU toggle) if `recover: power` was requested. Reset attribution then flows through the normal reset-marker machinery.

The agent always gets a truthful terminal state — `ok`, `echo_mismatch`, `no_prompt`, `hung` — with evidence attached; it never has to infer success from silence.

**Tests (added to §13 as suites `follow` and `runner`):** `follow` — each predicate type fires correctly; `any` returns first-match identity; cursor gap-freeness across rapid increments (property test); cursor across reconnect; `CURSOR_EXPIRED` after prune; timeout-returns-data; long-poll under concurrent multi-agent follows. `runner` — happy path; echo verification catches injected dropped/corrupted char; per-char retry succeeds then aborts correctly on repeat failure; no-echo console mode; command during boot (no prompt) → `NO_PROMPT` not a blind send; prompt scrolled by async kernel messages mid-command (prompt regex vs interleaved dmesg); each recovery rung individually (sleep-hang cured by Ctrl-C; cat-hang cured by Ctrl-D; hard-hang exhausts ladder → `hung` + hook fired); TX lock: two concurrent transactions serialize, output attribution never crosses; transaction output correctly linked when the command causes a panic; distinctive-prompt validation rejects `:`.

One optional low-level `send(device, data, hex?)` passthrough remains behind a config flag (default off) for escape hatches; everything routine goes through `run_command`.

### 8.4 Freshness, boot epochs, and the boot report

The staleness trap: an agent power-cycles a board (often out-of-band — PDU, labgrid, a finger), asks "did it boot?", and gets confidently answered from the *previous* boot's captured output. The spec closes this with three mechanisms:

**Boot epochs.** Every device's stream is partitioned into monotonically numbered `boot_id` epochs, opened by any of:
- **`mark(device, label?)`** — explicit: the agent calls this *before* flipping power; returns the new `boot_id` + cursor. This is the recommended dev-loop idiom, and it's automatic when power is driven through conminer's optional **`power(device, on|off|cycle)`** external-hook tool (the hook invocation itself opens the epoch and is recorded in it).
- **Detected reset** — the A.0/A.10 reset-marker machinery (earliest-stage banner recurrence, `rst:` reset-cause lines, watchdog attribution) opens an epoch automatically, so out-of-band power flips still get their own epoch, just without the agent-chosen label.
- All records, templates-occurrences, stages, and cursors are epoch-tagged; every query accepts `boot_id` (or `boot: latest`), and `list_boots(device)` shows the epoch history with outcomes.

**Freshness envelope on every response.** Every read-tool response carries `freshness: {server_now, last_rx_ts, idle_ms, boot_id, boot_opened_by: mark|reset|power, bytes_this_boot, capture_state}`. An agent that just powered a board and sees `boot_id` unchanged from its `mark`, or `last_rx_ts` predating it, *knows* it is looking at stale data — the answer self-describes its own currency instead of relying on the agent to reason about it.

**Silence with attestation.** "Was there any output?" has three truthfully distinct answers, and the tools never conflate them:
- `bytes_this_boot: 0` **with** `capture_state: listening (since <ts>)` — genuinely no output; the port was open, RX active, zero bytes arrived. Only claimable because minerd continuously attests capture health (port open, ser2net session up, read-loop alive, RX error counters).
- `capture_state: not_listening` (port vanished, ser2net down, discovery lost the device) — "I don't know" returned as a structured condition, never as "no output".
- `bytes_this_boot: N, classified garbage` — output arrived but failed the GARBAGE_BURST threshold (baud mismatch after a strap change is a *classic* fresh-power-on failure and gets named as such, not shown as silence or noise lines).

**`boot_report(device, boot_id=latest)`** — the one-call answer to "what happened?": classifies the epoch as `booted` (prompt/target stage reached, with time-to-prompt), `crashed` (crash record refs), `hung` (deepest stage reached, last record, silence duration since), `looping` (epoch chain count with the repeating template diff), `no_output` (with attestation), `garbage` (with byte stats), `booting` (§K5a: at least one stage entered, no terminal stage yet, and the console spoke within the last 15 s) or `in_progress` (§L5: the epoch is open and producing bytes but no stage banner has been recognised yet -- the honest name for "something is talking and I cannot yet say what it is"; `booting` is the same situation once a stage is known) — plus the stage timeline and novel-templates-this-boot. The dev loop becomes: `mark` → power on → `follow(until={any: [...]}, timeout)` → `boot_report`. Three calls, no raw log paging, no staleness ambiguity.

**Epoch fingerprints — identical boots, distinct identities.** Repetition never creates ambiguity because identity and similarity are handled by different layers:
- *Identity is positional:* every raw line carries host-monotonic receipt time + absolute byte offset in the device's append-only stream. Byte-identical boot iterations are distinct objects at distinct offsets under distinct `boot_id`s; cursors are offsets, so a query anchored after a `mark` cannot see a previous iteration's lines regardless of content equality. Storage never dedupes — only the template layer (a derived view) collapses repetition.
- *Similarity is fingerprinted:* each epoch gets `fingerprint = hash(ordered template-ID sequence ‖ stage timeline)`. Deliberately semantic, not a raw hash — raw bytes of two loop iterations never match (timestamps differ every pass), but the template sequence, with variable fields already wildcarded, matches exactly when the boot behaved identically. `looping` classification = a chain of equal fingerprints (`stable since epoch <n>`); a *broken* chain is the debug signal — one novel template or one stage further and the new epoch's fingerprint diverges, with `diff_sessions` naming the delta. "Same crash again" vs "different crash now" is a hash comparison, not a log read.
- `list_boots` and `boot_report` surface `fingerprint`, `fingerprint_stable_since`, and `first_divergence_from_previous`.

**Tests (suite `epochs`, added to §13):** mark-then-power ordering; auto-epoch on detected reset with no mark; out-of-band double power-cycle creates two epochs; query with stale `boot_id` returns that epoch (not latest) and says so; freshness envelope monotonicity (property test); `no_output` requires listening attestation — port-down during the window forces `not_listening` instead; garbage classification on baud-mismatch corpus; `boot_report` classification for each outcome class incl. `looping` across 500 epochs; epoch tagging consistency between records/templates/stages (property test); `power` hook failure surfaces as structured error, epoch not opened; fingerprint equality across byte-identical replayed epochs (property test: same corpus replayed twice ⇒ equal fingerprints, distinct boot_ids and offsets); fingerprint divergence on single-template injection mid-loop with `first_divergence_from_previous` pointing at it; fingerprint invariance to wildcarded fields (timestamps/addresses perturbed ⇒ fingerprint unchanged).

### 8.5 Perceived console state + prompt expectations

**Console state machine.** minerd continuously maintains a `console_state` per device — the system's best current belief about what the console *is doing* — derived from the epoch/fingerprint/stage/prompt machinery already in place. It is surfaced in every freshness envelope, in `list_devices`, and as a notification on transition, so agents get told the perceived state rather than deducing it:

| State | Derived from |
|---|---|
| `no_signal` | zero bytes + listening attestation |
| `garbage` | GARBAGE_BURST active (baud mismatch etc.) |
| `booting(stage)` | inside an epoch, stage machine progressing |
| `at_prompt(kind, pattern, stage)` | prompt detected, line idle — `kind ∈ {bootloader, shell, rtos_shell, monitor}` |
| `login_wait` | `login:` / `Password:` matched — deliberately distinct from `at_prompt`: the board is up but **not commandable** without credentials; `run_command` refuses with `LOGIN_REQUIRED` instead of typing into a login field |
| `in_command(txn_id)` | runner transaction active |
| `streaming` | steady non-boot output (app logs), no prompt |
| `hung(stage, silent_ms)` | epoch stalled: no bytes, no prompt, stage incomplete |
| `boot_looping(kind, count, fp)` | epoch chain analysis — see taxonomy |
| `unstable` | fingerprints diverging without progress (flaky) |

**Loop taxonomy** (the "similar patterns"), all from fingerprint-chain + stage analysis, all reported with count, stability span, and deepest stage:
- **stable loop** — identical fingerprints (`fp stable ×N`): deterministic failure, diff-vs-known-good is the move
- **crash loop** — each epoch reaches stage S then emits a crash record: boots *then* dies (vs never boots)
- **stage-capped loop** — epochs never pass stage S, no crash record (silent death in, e.g., BL31 → DEAD_AIR close each time)
- **watchdog cycle** — reset attribution = watchdog each epoch (WATCHDOG class / `rst:` cause): distinguishes "it hangs and gets shot" from "it crashes"
- **flapping** — fingerprints alternate or diverge epoch-to-epoch: nondeterministic/flaky, race or hardware-marginal signal; `boot_report` includes the fingerprint histogram
- The state answers contamination directly: while `boot_looping`, `latest`-scoped queries default to the current epoch only, and `boot_report` summarizes the chain — 400 identical epochs are one line of state, zero pollution of the current question.

**Prompt expectations — queryable, per stage, learned.** The §8.3 prompt registry is extended into a per-stage expectation table, and exposed:
- **`get_prompts(device, stage?)`** returns, per boot stage, the expected prompt set with provenance and confidence: `profile` (e.g. `=> ` at stage uboot, `uart:~$ ` at zephyr, `grub> `, EC `> `), `configured` (device config / image metadata supplied by the user), and `learned` — prompts minerd actually observed the device settle at, with `last_seen`, observation count, and the stage they appeared in. So the agent's question "what prompt should I expect when this image is booted / when it reaches U-Boot?" is a direct tool call, answered from the image's own history once one good boot has been observed.
- Learned prompts are first-class data: they feed `follow`'s `prompt:true` predicate, `run_command`'s idle assertion, and `boot_report`'s `booted(at_prompt: <pattern>, time_to_prompt)` — and they are epoch-associated, so a *changed* prompt after flashing a new image (different PS1, different hostname) is itself surfaced as a delta rather than a detection failure.
- **Credential gates are patterns, not assumptions.** `login_wait` is driven by a configurable per-profile/per-device *credential-gate pattern set* (defaults: `login:`, `Username:`, `Password:`, common getty forms) — extendable through `profiles.d`/device config for custom greeters, localized strings, BMC variants, or bootloader password prompts, and removable entirely for autologin images (which simply settle at `at_prompt`). No specific string is baked in.
- **Unknown gates fail honest, not wrong.** A line the console visibly idles at that matches *neither* a known shell prompt nor a known credential gate classifies as **`at_unknown_prompt(observed_line)`** — "something is waiting for input and I don't know what." `prompt:true` does not fire, and `run_command` refuses with `UNKNOWN_PROMPT` (overridable with `force: true`) — the never-type-into-an-unknown-state rule holds. The agent inspects the tail and can teach the system via **`classify_prompt(device, pattern, kind ∈ {shell, bootloader, rtos_shell, monitor, credential_gate, ignore})`**; the classification persists as a `learned` registry entry, so each unfamiliar console is a one-time teaching event, not a recurring misclassification.
- `login_wait` handling is otherwise as above: classified gates are never shell prompts; optional per-device credentials let `run_command` (or a `login(device)` helper) traverse the gate deliberately, recorded as a transaction like any other.

**Tests (suite `state`, added to §13):** every state reachable and correctly derived from replayed corpus; each loop-taxonomy class classified correctly (incl. watchdog-vs-crash distinction and flapping via alternating fingerprints); default credential-gate set triggers `login_wait`, and a custom greeter does so only after config/teach; autologin corpus goes straight to `at_prompt` with no gate state; unfamiliar idle line → `at_unknown_prompt`, `prompt:true` withheld, `run_command` → `UNKNOWN_PROMPT` (and succeeds with `force`); `classify_prompt` teaching persists across minerd restart and reclassifies subsequent boots; no gate pattern ever satisfies `prompt:true`; learned prompt appears in `get_prompts` after first observed settle, with correct stage; changed-PS1 image produces prompt delta not detection failure; state transitions emit exactly one notification each; `latest` query scoping during `boot_looping` stays within current epoch; state survives minerd restart (rebuilt from store, not RAM).

**Response budget discipline:** every tool has hard caps and returns totals + cursors, never unbounded dumps — the entire point is that a looping kernel crash comes back as `{template, count: 41283, first_ts, last_ts}` plus three examples, not 41,283 lines.

### 8.1 Search subsystem (indexed + multiline)

`search_raw` alone is an unindexed line-scan; that's insufficient at 800 MB and blind to matches spanning line boundaries. The search subsystem is therefore three tiers behind one tool:

**Tool:** `search(query, mode, scope, session|device, max_results, cursor)`
- `mode = terms` — all words present (any order) · `phrase` — exact contiguous phrase · `regex` — full regex, `(?s)` permitted.
- `scope = line` — match within single raw lines · `record` — match within a framed record's full text (an oops, a panic dump, a traceback as one unit) · `window(n)` — sliding n-line window across the session for spans that cross record boundaries (default n=20, capped).
- Results: matched line/record IDs + highlight offsets + `get_context`-ready anchors, capped and cursored like every other tool. `search_raw` remains as a thin alias (`mode=regex, scope=line`) for uart-mcp parity.

**Tier 1 — FTS index (terms/phrase):** SQLite **FTS5** tables over both `raw_lines` and `records` (record text = its lines joined with `\n`, so multiline phrase search works natively at record scope — and since the framer already groups every crash into one record, "search for this two-line panic message" is the *indexed* fast path, not the fallback). Unicode tokenizer with no stemming (log text is not prose; `errno` must not match `error`), prefix indexes for partial-token queries. Populated incrementally on the live path and in-batch during ingest.

**Tier 2 — regex at line/record scope:** regex runs over the candidate set FTS can pre-narrow (literal fragments extracted from the pattern), falling back to full scan only for literal-free patterns. Rust's `regex` crate: linear-time, no catastrophic backtracking from hostile patterns.

**Tier 3 — windowed cross-boundary scan:** `scope=window(n)` streams the session through an n-line overlap buffer for matches that cross record boundaries (rare, but e.g. a sentence split by an interleaved dialect line). Explicitly the slow path; the response marks it `scan=true` with bytes scanned.

**Cost accounting:** FTS roughly doubles per-device storage; it's on by default, disable-able per device (`index=off` → tiers 2/3 only), and `stats` reports index size. Index build is idempotent and rebuildable from raw (same guarantee as templates, §6).

**Tests (added to §13 as suite `search`):** phrase spanning 2/5/40 lines inside one record; phrase crossing a record boundary (window scope finds it, record scope correctly doesn't); terms vs phrase distinction; regex with and without extractable literals (pre-narrow vs scan); hostile regex (nested quantifiers — must stay linear); unicode + invalid-UTF-8 lines in the index path (indexed lossily, always findable via tier 2/3 raw scan); `index=off` degradation; incremental index equals batch-rebuilt index (property test); FTS result parity with grep ground truth over corpus; cursor stability during live writes; 800 MB session: indexed phrase query returns in <100 ms, windowed scan bounded and reported.

## 9. Docker / compose deliverables

- `Dockerfile` — multi-stage: `rust:alpine` builder (musl, x86_64 + aarch64) → `FROM alpine:3.20` runtime with `ser2net` and `eudev` packages; one image, four entrypoints (or one binary with subcommands: `conminer discoveryd|minerd|mcpd|ingest`).
- `docker-compose.yaml` — the four services; `/dev` and `/run/udev` mounted into discoveryd; `devices:` cgroup rules for `/dev/tty*`; named volume `conminer-data`; mcpd published on `:8090`; healthchecks; `restart: unless-stopped`.
- `profiles.d/` mounted as a bind volume so framer profiles are editable without rebuilds.
- Example `claude_desktop_config.json` / Claude Code `.mcp.json` snippets for both HTTP and `docker exec` stdio attachment.

## 10. Phased implementation plan

| Phase | Deliverable | Notes |
|---|---|---|
| **1. Core library + CLI** | Drain (no-masking) + SQLite store + `conminer ingest file.log` + `conminer templates` CLI | Proves compression on real Qualcomm/LAVA logs; benchmark 800 MB target |
| **2. Framers** | Profile engine + `linux`, `uboot`, `zephyr`, `raw` profiles + boot-stage machine | Test corpus: collected boot logs per stage; golden-file framing tests |
| **3. MCP server** | mcpd with full read-side tool surface over Phase-1 store | Usable for post-hoc workflows immediately (LAVA artifacts) |
| **4. Live + compose** | minerd live pipeline, discoveryd, ser2net generation, full docker-compose | The auto-discovery "plug in a cable, agent sees it" demo |
| **5. Breadth + diffing** | `uefi`, `tfa`, `optee`, `freertos`, `threadx` profiles; `diff_sessions`; novel-template notifications | HIL/soak-test integration point |
| **6. Upstream + ecosystem** | Publish crates + images; PR proposal to uart-mcp (`get_log_templates` backed by lib); labgrid exporter RFC | Also: LAVA triage agent adopts `ingest_file` |

## 11. Risks / open items

- **FreeRTOS/ThreadX framing** has no standard log format; ship heuristics but set expectations that per-project profiles are the norm (declarative tier makes this cheap).
- **Interleaved SMP oops lines** can arrive out of order; v1 frames greedily and flags suspected interleave rather than reassembling.
- **Template fragmentation without masking** — mitigated per §6; monitor template-count growth as a health metric and surface merge suggestions.
- **udev inside containers** varies by host distro; fallback discovery mode polls `/dev/serial/by-id` at 1 Hz if netlink is unavailable.
- **drain-rs maturity** — evaluate in Phase 1; owning ~300 LoC is the safe default.

---

## 12. Test harness

The workspace ships a dedicated `testkit` crate providing shared infrastructure for every other crate's tests. Six layers, cheapest first; CI runs 12.1–12.4 on every PR, 12.5 nightly, 12.6 as phase gates.

### 12.1 Unit + property tests
- Standard `cargo test` per crate, plus **property-based tests** (`proptest`) for the core invariants:
  - **Raw preservation:** for any byte sequence `x`, `read(store(x)) == x` — byte-for-byte, including invalid UTF-8, nulls, ANSI, torn lines. This is the no-masking guarantee as an executable property.
  - **Framer conservation:** framing never drops, duplicates, or reorders lines; concatenating all record spans reproduces the input line sequence exactly.
  - **Drain determinism:** identical input order ⇒ identical template set and IDs.
  - **Rebuild idempotence:** `rebuild_templates(raw)` twice ⇒ identical output; template store is provably a derived view.
  - **Chunking independence:** splitting the input byte stream at arbitrary boundaries (mid-line, mid-UTF-8-codepoint, mid-escape-sequence) never changes framing or template output.
- **Fuzzing** (`cargo-fuzz`) on the three untrusted-input surfaces: line splitter, tokenizer, framer state machines. Serial input is hostile by definition (baud mismatch = structured garbage). Fuzz targets run continuously in CI with a corpus checked into the repo.

### 12.2 Golden-corpus tests
- `corpus/` (git-lfs): real captured logs per firmware — Linux boot + oops/panic/OOM/lockdep samples, U-Boot (incl. exception dumps and the backspace-rewriting autoboot countdown), UEFI/EDK2 with ANSI, TF-A BL1→BL31, OP-TEE, Zephyr fatal errors, FreeRTOS/ThreadX project samples, plus public loghub datasets for Drain accuracy baselines.
- **Snapshot testing** (`insta`): each corpus file has committed golden output — record boundaries, stage transitions, template set with counts. Any framing or mining change produces a reviewable diff, never a silent behavior shift.
- Corpus contribution is the primary way new firmware support lands: a profile PR **must** include corpus samples + goldens for every edge case it claims to handle.

### 12.3 Replay + fault-injection engine (virtual serial)
- `testkit::replay` streams corpus bytes through real ptys (`socat`-created pairs, or `openpty` directly) and through a real ser2net instance, exercising the identical live pipeline as hardware — no hardware required, fully deterministic.
- Timing profiles: byte-at-a-time, bursty, baud-paced, multi-hour soak compression (replay 72 h of timestamps in minutes with tokio paused time).
- Fault injection between corpus and pty: random bit flips (line noise), garbage bursts (wrong baud), truncated lines, device disappear/reappear mid-record, EAGAIN storms, reconnect races.
- Assertions: pipeline never panics, never deadlocks, quarantines garbage as `raw`-profile records, resumes cleanly after reconnect, and accounts for every byte (received == stored + explicitly-dropped-with-counter).

### 12.4 Integration: storage, tools, MCP conformance
- **Crash safety:** `kill -9` minerd at randomized points during live capture and during an 800 MB ingest; assert SQLite WAL recovery, zero corruption (`PRAGMA integrity_check`), no lost committed records, ingest resumability.
- **Concurrency:** N parallel device pipelines + M concurrent `ingest_file` jobs + query load; assert isolation (session boundaries mid-record, `end_session` racing a live writer, retention pruning racing an open cursor — cursor completes or fails cleanly, never returns corrupt pages).
- **MCP conformance:** spin up mcpd, drive it with a real MCP client; JSON-schema-validate every tool response; verify hard caps and cursors on every tool (adversarial calls asking for unbounded output get bounded output); verify notification delivery for novel-template and stage-transition events; verify parity tools match uart-mcp response shapes.
- **Migration tests:** fixture databases from every released schema version; upgrade must succeed and pass integrity + golden queries.

### 12.5 Performance + endurance (nightly)
- `criterion` benches with CI regression gates: ingest throughput (floor: 100 MB/s on the reference runner), per-line live-path latency, template lookup, `list_templates` on a 10 M-record session.
- The full 800 MB benchmark file, plus 10 KB smallest-case (fixed-cost check).
- **Leak/soak:** 24 h replay loop (compressed clock), RSS and SQLite file-growth ceilings asserted; template-count growth monitored as the fragmentation health metric (§6).

### 12.6 End-to-end compose gates (per phase)
- `testcontainers`-driven: build the real Alpine images, bring up the full compose stack, simulate hotplug by creating/removing pty symlinks in a faked `/dev/serial/by-id`, and assert the demo contract: **device visible to `list_devices` and mined within 2 s of plug-in**, survives ser2net reload, survives `docker restart` of each service independently.
- Runs on x86_64 and aarch64 runners (lab hosts are both).
- Each phase in §10 gains an explicit gate: Phase N is done when its feature suites (§13) pass in this environment.

## 13. Test suite catalog (per feature × edge cases)

Every suite below is a named directory under `tests/`; the edge-case lists are the checklists PRs are reviewed against, and each listed case is at least one test.

| Suite | Feature | Edge cases (each = a test) |
|---|---|---|
| `discovery` | discoveryd | hotplug add/remove; rapid replug; same adapter re-enumerating with new ttyUSBn but same by-id; two identical adapters (serial-less FTDI clones → positional fallback); udev unavailable → 1 Hz poll fallback; permission-denied device; symlink churn during scan |
| `ser2net-gen` | config generation | 0/1/32 devices; device removed while consumer attached; SIGHUP reload race; port-number stability across restarts; malformed nickname chars |
| `linesplit` | byte→line | \n, \r\n, \r-only, mixed within one stream; no trailing newline; 1 MB single line (cap + continuation flag); null bytes; invalid UTF-8; split mid-codepoint across reads; ANSI escapes; backspace/CR overwrite sequences (U-Boot countdown renders as final text, bytes preserved raw) |
| `framer-linux` | linux profile | single printk; nested oops in irq context; `Call Trace:` continuation; truncated panic (power loss mid-record → record closed with `truncated` flag); interleaved SMP lines (flagged per §11); OOM report; lockdep splat; loglevel-less lines; `printk` time going backwards after RTC sync |
| `framer-uboot` | uboot profile | SPL→U-Boot handoff; exception dump; autoboot countdown; env dump; interrupted boot (keypress) |
| `framer-uefi` | uefi profile | ANSI-heavy output; ASSERT with context dump; progress spinner overwrite |
| `framer-tfa` / `framer-optee` / `framer-zephyr` / `framer-freertos` / `framer-threadx` | each profile | banner detect; fatal/panic record with full dump; profile-specific severities; mid-record stream loss |
| `stagemachine` | boot-stage tracking | full BootROM→TF-A→U-Boot→kernel chain; boot **loop** (stage cycle × 500 — dedup across iterations, bounded stage rows); watchdog reset mid-kernel; pinned profile disables detection; ambiguous banner |
| `drain` | template mining | identical lines dedup; token-count variance fragmentation (documented behavior locked by test); high-cardinality hex convergence to `<*>`; unicode tokens; similarity threshold boundary at 0.4; max-children overflow node; per-profile tokenizer rules (`key=value`); merge-suggestion generation never mutates templates |
| `store` | persistence | WAL crash points; retention pruning (size, age, keep-all file sessions); prune vs open cursor; 2 GB live cap enforcement; template/occurrence rollup correctness; per-device isolation; disk-full behavior (fail loud, stop capture, health flag — never silent drop) |
| `ingest` | post-hoc | 10 KB; 800 MB; gzip’d input; file with BOM; concurrent ingests same device; duplicate re-ingest (idempotent by content hash → new session, warning); nonexistent path; permission denied |
| `tools` | every MCP tool | happy path; empty result; cap enforcement; cursor stability under concurrent writes; invalid args (structured error, §14.6); `new_only` correctness across sessions; `diff_sessions` on disjoint devices (error) |
| `notify` | notifications | novel template fires exactly once; stage transition ordering; subscriber disconnect/reconnect; notification storm during boot loop (coalescing) |
| `e2e` | compose stack | §12.6 contract; independent service restarts; volume backup/restore round-trip |

## 14. Gap analysis — what was missing

Items absent from v0.1, now in scope (spec deltas below are normative):

1. **Security.** Authentication is explicitly out of scope for now (revisit before any deployment beyond the lab segment). mcpd binds `127.0.0.1` by default and exposing it wider is a one-line config change; the optional `send` passthrough stays behind its config flag. Threat model note retained: serial input is untrusted regardless.
2. **Observability of the observer.** Structured JSON logs (to stderr, honoring the it-must-not-eat-its-own-tail rule: conminer's logs are excluded from mining by default); Prometheus `/metrics` (bytes/s per device, records/s, template count, queue depth, dropped-byte counters, SQLite sizes); `/healthz` per service wired to compose healthchecks.
3. **Backpressure policy.** A 4 Mbaud tracing console can outrun SQLite. Bounded per-device queues; on overflow, raw bytes continue to the on-disk ring (capture never stops) while mining degrades to sampled mode, with a loud health flag and a counter — *never* silent loss. Test in `replay` suite with a firehose profile.
4. **Time model.** Every line gets a host-monotonic and host-wall timestamp at receipt; target-side timestamps (printk time, RTC lines) are extracted fields, never trusted for ordering. Clock-jump and NTP-step cases in `framer-linux` and `store` suites.
5. **Config schema.** One `conminer.toml` (env-overridable) validated by `conminer check-config`; invalid config fails fast at startup, and the compose healthcheck catches it.
6. **Error taxonomy.** All tool errors are structured (`code`, `message`, `hint`) so agents can branch on them — e.g. `DEVICE_GONE`, `SESSION_ACTIVE`, `RESULT_CAPPED`, `INGEST_TOO_LARGE`.
7. **Export/import.** `export_session(session_id)` produces a single portable archive (raw + metadata) for bug reports and cross-lab sharing; `ingest_file` accepts it. Round-trip test in `store` suite.
8. **Schema migrations.** Embedded, versioned, forward-only migrations from day one (even pre-1.0), with the fixture-DB upgrade tests of §12.4.
9. **Release engineering.** Tagged releases build multi-arch (x86_64/aarch64 musl) signed images + SBOM; `CHANGELOG.md` gated in CI; corpus and goldens versioned with the code so any release is reproducible.
10. **Backup/runbook.** Documented: volume snapshot = complete backup (SQLite WAL checkpointed on schedule); restore procedure tested in `e2e`.
11. **Profile authoring guide.** The declarative-framer doc plus a `conminer profile test <profile> <corpus-file>` command so firmware teams can develop profiles against their own logs without touching Rust.

### Phase-gate amendment to §10
Each phase now ends with: **(a)** its §13 suites green in the §12.6 environment, **(b)** no criterion regression beyond threshold, **(c)** fuzz corpus clean for 24 h. Phase 1 additionally establishes the CI skeleton (fmt, clippy -D warnings, coverage floor 85% lines / 100% framer state transitions) so the rule "every feature and edge case ships with tests" is enforced by machinery, not discipline.

---

## 15. Gap analysis, round 2 — what v0.2 still doesn't cover

Reviewed against the full dev-loop an agent actually runs (flash → power → boot → interact → crash → diagnose → repeat) and against multi-agent lab reality. In rough priority order:

1. **Device reservation (multi-agent labs).** The TX lock serializes *transactions*, not *intent*: two agents can interleave whole workflows (one flashing while another runs a soak test). Add labgrid-style `acquire(device, lease_ttl)` / `release`, with reads always unrestricted, mutating tools (`run_command`, `power`, `flash`, `mark`) requiring the lease, lease expiry + steal-with-notification semantics. Where a labgrid coordinator already owns arbitration, conminer defers to it (integration, not duplication).
2. **Port handoff & binary-protocol awareness.** Flashing tools (QDL/Sahara, fastboot-over-serial, lrzsz, kermit, GDB stubs) need the raw port. Add `claim_exclusive(device)` → miner keeps *capturing* bytes (nothing is ever lost) but suspends line/framing interpretation and marks the span `binary(protocol?)`; protocol sniffers (Sahara handshake, GDB `$...#xx` packets, zmodem `rz\r**`) set the state so a Sahara dump is labeled as such, not garbage-classified. Release resumes framing at a clean boundary.
3. **Flash workflow hook.** Like the power hook: per-device `flash(device, image_ref)` external-command hook (fastboot/dfu-util/QDL invocation is the lab's business, not conminer's) — but the *epoch machinery* records it: flash events open a provisioning span, bind image metadata to subsequent epochs, and `boot_report` can answer "first boot after flash."
4. **Build-identity correlation.** Epoch diffs are positional (#443 vs #444); the question is "build A vs build B." Bind image identity to epochs: explicit (`set_image(device, {git_sha, image_hash, name})`, or supplied via the flash hook) plus automatic banner extraction (kernel version string, U-Boot build tag, Zephyr build hash). Then `diff_builds(image_a, image_b)` aggregates across all epochs of each build — the regression question answered directly, and the LAVA-triage-agent's meta-qcom-commit correlation gets its hook.
5. **Symbolization service (optional sidecar).** Crash records carry raw addresses; agents want function names. `symbold`: upload/point-at symbol artifacts per image (vmlinux, System.map, ELFs, Zephyr .elf), `symbolize(record_id)` resolves backtrace/register addresses (addr2line/gimli), results cached and attached as derived annotations — never mutating raw (consistent with §6). Kernel module offset handling and KASLR (needs `Modules linked in` + relocation base from the record itself) in scope; explicitly best-effort.
6. **pstore/ramoops ingestion.** The console can miss a crash the kernel preserved: after reboot, `/sys/fs/pstore` holds the previous oops/panic. A `run_command`-based collector (or agent-invoked `ingest_pstore(device)`) pulls and ingests it into the *previous* epoch with `source=pstore`, deduplicating against what the console did capture. Same treatment for `mtdoops`/`ramoops` regions and — via file ingest — kdump/minidump text portions.
7. **LAVA-native ingestion.** `ingest_file` learns LAVA's YAML log format (timestamps, levels, feedback streams) as a first-class input codec, mapping LAVA actions to stage hints. Closes the loop for the Foundries corpus and the triage agent without preprocessing scripts.
8. **Multi-console targets.** One DUT is often several UARTs (AP + EC, BMC + host, secure + normal world on separate ports). Add `target` grouping: N devices under one logical target, epochs correlated across members (power event on one opens epochs on all), and cross-console queries — `get_context(target, around=<record>)` interleaves all member consoles by host-receipt time, answering "what did the EC see when the AP panicked."
9. **Serial line control + auto-baud recovery.** Expose RFC2217 line-parameter control (`set_line(device, baud, parity, ...)`) — needed when bootloader and kernel consoles run different rates — and optional auto-baud probing: on sustained GARBAGE_BURST, cycle common rates until printable ratio recovers, recorded as a line-config event (default off; it perturbs the port).
10. **File transfer over serial.** Console-only boards still need artifacts moved: `push_file`/`pull_file` via pluggable strategies (lrzsz X/Y/ZMODEM when present, `base64`+`cat`/`dd` fallback with checksum verification, U-Boot `loady`). Built on the runner's transaction discipline; integrity verified end-to-end; sizes bounded.
11. **CI gating policy.** For LAVA/HIL pipelines: declarative policy docs — allowlists of known/accepted templates and fingerprints (flaky known issues), severity thresholds, "fail only on *novel* crash" mode — evaluated by `evaluate_policy(session, policy)` returning a machine verdict + human summary. This is the piece that turns the miner from diagnostic tool into regression gate.
12. **Agent-level eval scenarios.** Per the project rule, one more test tier: scripted end-to-end *debugging scenarios* (boot loop, stage-capped hang, flaky flap, panic-after-flash, login-gated console) where an LLM agent, given only the MCP surface, must reach the correct diagnosis; assertions on the tool-call transcript and final answer. Catches tool-ergonomics regressions no unit test sees — run nightly, not per-PR (nondeterministic tier, tracked statistically like the fuzz tier).
13. **Explicit non-goals (documented):** full-screen TUI interaction over the runner (menuconfig, vi) — line-oriented transactions only; masking/redaction of any kind (§6 stands); symbol-less binary reverse-engineering; being a flashing tool (hooks only); replacing labgrid/LAVA scheduling (integrate, defer).

Phase placement: items 1–3 join Phase 4 (they gate multi-agent live use); 4, 6, 7, 11 join Phase 5 (they're what the LAVA/HIL integration actually consumes); 5, 8, 9, 10, 12 form a new Phase 5.5; item 13 lands in the README from day one. Each arrives with its §13 suite per the project rule.

---

## 16. Configuration reference — every knob, every default

Everything lives in `conminer.toml` (env-overridable, `conminer check-config` validated, §14.5). Defaults chosen for a lab host; each notable choice carries its rationale. Per-device overrides exist for anything marked ⌂.

**Discovery & attachment**
| Key | Default | Notes |
|---|---|---|
| `discovery.include` / `discovery.exclude` | include `*`, exclude empty | **Glob lists over by-id names.** The exclude list is the important one: lab hosts have modems, UPS serials, debug probes' aux ports — conminer must be *keepable off* devices it doesn't own. Excluded devices are listed as `ignored`, never opened |
| `discovery.hotplug_debounce_ms` | 500 | Cheap hubs bounce enumeration |
| `discovery.poll_fallback_hz` | 1 | When udev netlink unavailable (§11) |
| `ser2net.base_port` | 5001 | Stable per-device assignment, persisted |
| `attach.reconnect_backoff` | 250 ms → ×2 → max 15 s | ser2net drop/reconnect; capture-state `not_listening` while down |
| `attach.tcp_keepalive_s` | 10 | Detect half-dead ser2net sessions |

**Line (⌂, §3.2)** — `line.baud=115200`, `8N1`, `flow=none`, `tx_line_ending="\n"` ⌂, `auto_baud=off`.

**Capture, sessions & durability**
| Key | Default | Notes |
|---|---|---|
| `capture.ring_mb` ⌂ | 64 | On-disk raw ring per device |
| `capture.commit_interval_ms` | 250 | **Durability window:** batched SQLite commits; a host power cut loses at most this much captured data. Tunable to 0 (per-line fsync) for paranoid rigs at a throughput cost — the trade is documented, not hidden |
| `capture.encoding` | raw + UTF-8-lossy view | Bytes stored verbatim always; display view replaces invalid sequences |
| `session.autosplit_quiet_s` ⌂ | 300 | Silence gap that closes a live session |
| `session.max_hours` | 24 | Rollover for month-long soak consoles |
| `retention.live_cap_gb` ⌂ / `retention.file_sessions` | 2 / keep-all | §7 stands |

**Framing & mining (⌂ per profile)**
| Key | Default | Notes |
|---|---|---|
| `framer.lookback_lines` | 64 | Retro-attach depth (Zephyr fault lines, Python tracebacks) |
| `framer.max_record_lines` | 2000 | Runaway-record cap; closes with `truncated` flag |
| `framer.record_timeout_s` | 10 | Open record with no continuation closes (DEAD_AIR interacts per profile: UEFI/TF-A profiles raise it) |
| `framer.garbage_threshold` | 30% non-printable over 512 bytes | GARBAGE_BURST trip point |
| `mine.similarity` / `mine.depth` / `mine.max_children` | 0.4 / 4 / 100 | Drain parameters, §6 |
| `mine.max_line_tokens` | 128 | Longer lines mined on the first 128 tokens, stored whole |
| `search.fts` ⌂ | on | §8.1 |

**Interaction (⌂)**
| Key | Default | Notes |
|---|---|---|
| `runner.char_delay_ms` | 10 | §8.3; slow bootloader consoles override upward |
| `runner.echo_timeout_ms` | 200 | Per-char echo wait |
| `runner.command_timeout_s` | 30 | Prompt-return deadline before recovery ladder |
| `runner.settle_quiet_ms` | 500 | Idle confirmation before send |
| `runner.escape_set` | `[C-c, C-\, C-d]` | Ladder rungs 2–3, ordered, ⌂ (`~.`, `C-]` for odd consoles) |
| `state.hung_after_s` ⌂ | 30 | Silence during incomplete boot → `hung` |
| `state.loop_min_epochs` | 3 | Epochs before `boot_looping` claimed |
| `lease.ttl_s` / `lease.max_s` | 900 / 14400 | §15.1; expiry notifies, steal requires explicit flag |
| `hooks.power_timeout_s` / `hooks.flash_timeout_s` | 30 / 600 | External hook deadlines; timeout = structured error, never hang |
| `credentials.file` | unset | Per-device login creds (§8.5) live in a **separate 0600 file**, never in the registry DB, never in `export_session` output, never echoed into stored TX bytes (the one deliberate exception to store-everything: password chars sent are recorded as `<credential>` markers in the *transaction log*; the raw RX stream — which contains no password echo on sane consoles — remains verbatim) |

**Service & API**
| Key | Default | Notes |
|---|---|---|
| `mcpd.bind` / `mcpd.port` | 127.0.0.1 / 8090 | §14.1 |
| `api.max_raw_lines` / `api.max_results` | 200 / 100 | Per-response caps (§8); cursors beyond |
| `api.follow_timeout_max_s` / `follow.default_timeout_s` | 600 / 30 | Long-poll bounds |
| `api.max_concurrent_follows` | 64 | Per instance |
| `notify.coalesce_ms` | 5000 | Boot-loop notification storms collapse (§13 `notify`) |
| `export.max_gb` | 4 | `export_session` size guard |
| `ingest.max_gb` / `ingest.gzip` | 2 / auto | Beyond cap → `INGEST_TOO_LARGE` with split hint |
| `log.level` / `metrics.bind` | info / 127.0.0.1:9090 | §14.2; conminer's own logs excluded from mining |
| `time.store` | UTC, host-stamped | §14.4; rendering is the client's problem |

**Compose-level defaults (in the shipped `docker-compose.yaml`):** pinned image tags (never `latest`); per-service memory limits (minerd 512 MB, others 128 MB — a leak OOMs one service, not the host); `restart: unless-stopped`; named volume; `/dev` + `/run/udev` only into discoveryd; tty device cgroup rules scoped by the include globs.

**Tests (suite `config`, added to §13):** every key above has a default-applied test and an override test; exclude-glob device never opened (and listed `ignored`); commit-interval durability property (kill -9 loses ≤ interval of data, §12.4 crash rig); max_record_lines truncation flag; hook timeout → structured error; credentials never appear in store, export, or logs (scanner test over all artifacts); check-config rejects unknown keys (typo protection) and out-of-range values with the offending line.

---

## 17. The human dashboard (added post-v0.2)

Everything above answers to an agent. `dashd` is the surface for the person at
the bench: a browser page that lists the consoles currently plugged in, hands
over each one's connection string, and lets a human watch a console live and
type into it.

**Live device list.** The page is driven by server-sent events off the registry
discoveryd already maintains, so a replug reaches the browser in about the time
discoveryd takes to notice it. An event is emitted only when the device set
*actually changes*, not on a timer: a steady lab generates no traffic, and the
change animation therefore means something.

**Connection budget.** ser2net fans one UART out to a small number of TCP
clients (eight by default) and minerd permanently holds one for capture. `dashd`
therefore opens **one** connection per device and fans it out to every browser
over a WebSocket: ten viewers cost the console one client, not ten, and the
remaining slots stay free for minicom, labgrid and anything else the lab
attaches. `max-connections` is emitted into the generated ser2net config, since
ser2net otherwise refuses the second client outright.

**Transmit is deliberately not mined.** Bytes typed in the browser go straight
down that TCP connection. They cost no latency and work even when minerd is
wedged; the trade is that an agent reading the mined stream sees the console's
echo with no record of who caused it. Nothing arbitrates access either: two
browsers, or a browser and an agent, can interleave keystrokes into one UART.
Both are deliberate, and the UI states them rather than implying a console is
exclusively yours. `dashboard.allow_tx = false` makes the whole surface
read-only.

**Exposure.** The dashboard binds the lab network with no authentication, which
is a deliberate choice for a trusted bench and the reason it is a separate
service on a separate port: a host that should serve only agents does not start
it.

**Tests (suite `dash`, added to §13):** driven against a real TCP listener
standing in for ser2net, because every property worth pinning is socket
behaviour — many viewers cost one client (including *simultaneous* first
viewers, which raced), transmitted bytes arrive verbatim, `allow_tx = false`
really drops them, a late joiner is replayed the scrollback, a console with no
endpoint is listed but not attachable, an unknown selector refuses the upgrade
instead of hanging, and the revision only moves when the device set really
changes.

---

## 18. Silicon bring-up (added post-v0.2)

Five capabilities that come from what an agent actually needs when it is the one
doing the bring-up rather than reading about it afterwards.

**18.1 The timeline spine.** A console is one evidence stream, and silicon
answers usually live in the join with another: a JTAG halt, a rail measurement,
a flash log, a CI result. Boot epochs are already numbered, timestamped and
fingerprinted, so `attach_evidence` lets any other tool hand conminer a fact with
a timestamp and have it land on the epoch that covers it. `timeline` then
interleaves evidence with stage transitions and crashes in time order, which is
what makes "the rail sagged 40 ms before it hung" a single read. Payloads are
stored verbatim: conminer does not understand a JTAG dump and must not pretend
to.

**18.2 Bisect.** "Which of these forty builds introduced the hang" is the most
expensive question in bring-up and almost entirely mechanical. conminer keeps the
bookkeeping (`bisect_start` / `bisect_report` / `bisect_status`) and can classify
a candidate straight from an epoch using a predicate, so the agent flashes, boots
and hands over a boot id. It remains **not a flashing tool**: the lab's own
tooling flashes, through the existing hooks. Two ways a bisect can lie are
reported rather than resolved: a range that is entirely skipped returns
`inconclusive` instead of naming a build, and a non-monotonic result (good after
bad) is surfaced as evidence the failure is intermittent.

**18.3 The decoder ring.** Consoles communicate in bare integers. `decode`
resolves errno names, AArch64 `ESR_ELx` exception classes and fault status, PSCI
codes, GIC INTID/SPI arithmetic (the off-by-32 between a device tree and a
register dump), and addresses against a per-device memory map. Per-device on
purpose: two boards on one host have different maps, and decoding against the
wrong silicon is worse than not decoding. Ambiguous values return **every**
reading with the assumption it rests on, because silently picking one is how an
agent ends up chasing the wrong interrupt.

**18.4 Absence detection.** Every other query answers "what appeared"; a bring-up
failure is usually the opposite shape. `learn_expectations` fits the skeleton of
a normal boot from reference epochs, and `missing_in_boot` reports what a suspect
epoch did not print. Reliability is carried rather than thresholded away (47-of-47
and 3-of-47 are different claims), a line that printed *late* is a separate
finding from one that never printed, and learning refuses to fit a skeleton to
one boot.

**18.5 The provenance guard.** The failure that invalidates everything downstream
of it is debugging a stale image because the flash did not take. `provenance`
compares what the console said about itself against what was last claimed to be
flashed, by hook or by `set_image`. The comparison is against the *console* only:
the epoch's bound image is a record of intent written by the same call that made
the claim, so counting it as agreement would make every check pass by
construction. A mismatch is only claimable when both sides are known; otherwise
the answer is `unknown`, never `match`.

**Tests (suites `spine`, `bisect`, `decode`, `absence`, `provenance`, added to
§13).**

---

## Appendix A — Pattern catalog (per project + generic abstractions)

Seed pattern sets for the framer profiles, surveyed across every target in scope. **Normative use:** each row becomes (a) a profile rule and (b) at least one golden-corpus test (§13). Strings below are seed knowledge; Phase-2 ports the exact regexes verbatim from each project's source (kernel `panic.c`/arch fault handlers, U-Boot `interrupts.c`/arch traps, EDK2 `CpuExceptionHandlerLib`, TF-A `crash_reporting`/`panic`, OP-TEE `core/kernel/panic.c`+`abort.c`, Zephyr `fatal.c`, LAVA `utils/messages.py`, SQUAD `linux_log_parser.py`) and locks them with corpus goldens — never trust a spec table over the source.

### A.0 The crash-record meta-grammar

Every project in scope emits fatal events with the same skeleton, which is the framer's generic state machine:

```
[BANNER]* → normal traffic → TRIGGER → CONTEXT-DUMP* → BACKTRACE* → TERMINATOR? → (RESET-MARKER | silence)
```

- **TRIGGER** — the line that opens a crash record (`Kernel panic`, `PANIC at PC`, `>>> ZEPHYR FATAL ERROR`, `ASSERT`, `Guru Meditation Error`…)
- **CONTEXT-DUMP** — register/state lines: dense, structurally regular, project-specific labels
- **BACKTRACE** — frame lines following a backtrace header
- **TERMINATOR** — explicit close (`---[ end trace`, `end Kernel panic`, `Halting system`, `Resetting CPU`) — when absent, the record closes on reset-marker, prompt, or timeout
- **RESET-MARKER** — the next stage banner (BootROM/SPL banner reappearing = involuntary reboot; this is how boot-loop detection works for free)

### A.1 Linux (`linux` profile)

**Banners / stage entry:** `Linux version <v> (` · `Booting Linux on physical CPU` · `] Kernel command line:` · `Run /init as init process` / `Run /sbin/init` · systemd/OpenRC first lines (userspace stage entry).

**Severity structure:** printk `<0>`–`<7>` loglevels when raw; bracketed monotonic timestamps `[ 1234.567890]`.

**Triggers (fatal):** `Kernel panic - not syncing:` (all variants: `VFS: Unable to mount root fs`, `Attempted to kill init!`, `Fatal exception`, `Fatal exception in interrupt`, `System is deadlocked on memory`, `Out of memory and no killable processes`) · `Oops:` / `Oops -` (x86 `Oops: 0000 [#1]`, arm64 `Internal error: Oops: <esr> [#1] PREEMPT SMP`) · `kernel BUG at <file>:<line>!` · `BUG: unable to handle kernel NULL pointer dereference` / `BUG: unable to handle page fault for address` · `general protection fault` · `invalid opcode:` · `Unhandled fault:` / `Unhandled prefetch abort:` (arm32) · `Bad mode in <x> handler detected` / `SError Interrupt on CPU` (arm64) · `Unable to handle kernel paging request` · `stack-protector: Kernel stack is corrupted`.

**Triggers (non-fatal but record-worthy):** `WARNING: CPU: <n> PID: <n> at <file>:<line>` · `------------[ cut here ]------------` (opens; pairs with end-trace) · `BUG: sleeping function called from invalid context` · `BUG: scheduling while atomic` · `watchdog: BUG: soft lockup - CPU#<n> stuck` · `NMI watchdog: Watchdog detected hard LOCKUP` · `INFO: task <t>:<pid> blocked for more than <n> seconds` (hung task) · `rcu: INFO: rcu_sched detected stalls` / `rcu_preempt detected stalls` · lockdep: `WARNING: possible circular locking dependency detected`, `possible recursive locking detected`, `inconsistent lock state` · `Out of memory: Killed process <pid>` + `oom-kill:` · `page allocation failure: order:` · sanitizers: `UBSAN:`, `KASAN:`, `BUG: KFENCE:`, `kmemleak:` · `refcount_t: underflow` / `overflow` · userspace: `potentially unexpected fatal signal`, segfault lines from `show_unhandled_signals`.

**Context-dump lines:** `CPU: <n> PID: <n> Comm:` · `Hardware name:` · `Modules linked in:` · x86 `RIP:`/`RSP:`/`RAX:` clusters · arm64 `pc :`/`lr :`/`sp :`/`x0 :`…`x29:` · arm32 `PC is at`/`LR is at`/`r0 :`… · `esr:`/`ESR:` decode lines · `Mem-Info:` blocks (OOM).

**Backtrace headers:** `Call Trace:` (x86) · `Call trace:` (arm64 — case matters) · `Backtrace:` / `Stack:` (arm32) — frames: `[<addr>] function+0xoff/0xsize` or ` function+0xoff/0xsize`.

**Terminators:** `---[ end trace <id> ]---` · `---[ end Kernel panic - not syncing: <msg> ]---` · `Rebooting in <n> seconds..` · `Fatal exception: panic_on_oops`.

**Edge behaviors to encode as tests:** timestamps jump at RTC sync; `earlycon` vs regular console duplication; `printk` rate-limit lines (`callbacks suppressed`); interleaved SMP oops (§11); panic inside panic (nested trigger before terminator).

### A.2 U-Boot (`uboot` profile)

**Banners:** `U-Boot SPL <ver>` · `U-Boot <ver> (<date>)` · `Hit any key to stop autoboot:` (backspace-rewritten countdown) · `Starting kernel ...` (stage exit → kernel).

**Triggers:** `data abort` · `prefetch abort` · `undefined instruction` · `"Synchronous Abort" handler, esr 0x<..>` (arm64) · `### ERROR ### Please RESET the board ###` · `initcall sequence <addr> failed at call <addr>` · `Wrong Image Format for <cmd> command` · `Bad Linux ARM64 Image magic!` · `ERROR: Did not find a cmdline Flattened Device Tree` · `FDT and ATAGS support not compiled in - hanging` · `alloc space exhausted` · `CACHE: Misaligned operation at range`.

**Context-dump:** `pc : [<addr>]  lr : [<addr>]` · `sp : <..> ip : <..> fp : <..>` · `x0 :`…(arm64 abort dump) · `Code:` line.

**Terminators / reset markers:** `Resetting CPU ...` · `resetting ...` · SPL banner reappearing (watchdog loop).

**Edge behaviors:** interactive prompt lines (`=> `) mid-stream; environment dumps (high-volume `key=value`, feeds the tokenizer rule); Ctrl-C interrupt of autoboot; `BOOTM` failure fallthrough to prompt.

### A.3 UEFI / EDK2 (`uefi` profile)

**Banners:** `UEFI firmware (version` · TianoCore DEBUG init lines · `[Bds]` phase markers · `BdsDxe: loading Boot....`.

**Triggers:** `ASSERT <file>(<line>): <expression>` · `ASSERT_EFI_ERROR (Status = <status>)` · `Synchronous Exception at 0x<addr>` (AArch64) · `!!!! X64 Exception Type - <n>(#PF - Page-Fault)  CPU Apic ID` (and IA32 variant) · `!!!! Find th' image based on IP` follow-ups · `DXE_ASSERT`/`PEI_ASSERT` forms.

**Context-dump:** `ExceptionData - <..>` · `RIP  - <..>, CS - <..>` register block (x86) · `X0 : <..>`…`X30` + `ESR : <..>` `FAR : <..>` (AArch64) · stack dump hex rows.

**Terminators:** none — EDK2 typically dead-loops (`CpuDeadLoop`); record closes on **silence timeout** or watchdog reset banner. This is the profile that proves the timeout-close path.

**Edge behaviors:** heavy ANSI (colors, cursor moves, progress spinners — CR-overwrite test); `DEBUG` verbosity flood at `DEBUG_VERBOSE`; mixed `\r\n` (UEFI_LINE_SEPARATOR) against `\n` from later stages.

### A.4 Trusted Firmware-A (`tfa` profile)

**Banners / stage entry:** `NOTICE:  BL1: v<ver>` · `NOTICE:  BL2: v<ver>` · `NOTICE:  BL31: v<ver>` · `NOTICE:  BL1: Booting BL2` — each BL is its own sub-stage for the stage machine.

**Severity prefixes:** `NOTICE:` `ERROR:` `WARNING:` `INFO:` `VERBOSE:` (fixed-width, easiest profile to classify).

**Triggers:** `PANIC at PC : 0x<addr>` · `Unhandled Exception in EL3` · `Unexpected BL31 exception` · `ASSERT: <file>:<line>` (or `ASSERT: <file> <line>` by build) · `ERROR:   Failed to load BL<n>` · exception-with-syndrome reports.

**Context-dump (crash reporting):** `x0  = 0x<..>` … `x30 = 0x<..>` aligned register file · `scr_el3`, `sctlr_el1`, `esr_el3`, `far_el3`, `spsr_el3`, `elr_el3` system-register lines · `cpuectlr` etc. implementation-defined rows.

**Terminators:** none typical — spins after panic; close on silence/reset. Reset marker: BL1 banner reappearing.

### A.5 OP-TEE (`optee` profile)

**Banners:** `I/TC: OP-TEE version:` · `I/TC: Primary CPU initializing` / `initialized`.

**Severity prefixes:** `F/TC:` `E/TC:` `I/TC:` `D/TC:` (core) · `E/LD:` (loader) · `E/TA:` etc. (TAs) — prefix encodes both severity and origin.

**Triggers:** `Panic '<msg>' at <file>:<line>` / `Panic at <file>:<line>` · `E/TC: ... Core data-abort at address 0x<addr>` (and prefetch/undef variants) · `TA panicked with code 0x<code>` · `assertion '<expr>' failed at <file>:<line>` · `E/LD:  Can't find <TA-uuid>`.

**Context-dump:** ` esr 0x<..>  ttbr0 ...` abort decode lines · `Call stack:` header with ` 0x<addr>` frame lines (raw addresses — symbolization is out of scope; record them verbatim) · `TEE load address @ 0x<..>` relocation hint line (needed by humans to symbolize — always keep inside the record).

**Edge behaviors:** OP-TEE shares the UART with the normal world — core messages interleave *inside* Linux output; the stage machine must support **nested/overlay dialects**, not just sequential stages. This is the profile that proves interleaved-dialect handling.

### A.6 Zephyr (`zephyr` profile)

**Banners:** `*** Booting Zephyr OS build <tag> ***`.

**Severity structure:** modern logger `<err>`, `<wrn>`, `<inf>`, `<dbg>` with module names (`<err> spi_nor:`), minimal-mode `E:` `W:` `I:` `D:` prefixes, optional `[hh:mm:ss.mmm]` timestamps.

**Triggers:** `>>> ZEPHYR FATAL ERROR <code>: <reason> on CPU <n>` — codes: 0 `CPU exception`, 1 `Unhandled interrupt`/spurious IRQ, 2 `Stack overflow`/stack check fail, 3 `Kernel oops`, 4 `Kernel panic` · `ASSERTION FAIL [<expr>] @ <file>:<line>` · Cortex-M fault decode lines that precede the fatal banner: `***** HARD FAULT *****`, `***** MPU FAULT *****`, `***** BUS FAULT *****`, `***** USAGE FAULT *****`, `Stacking error`, `Data Access Violation`, `Illegal use of the EPSR` · `Faulting instruction address (r15/pc): 0x<addr>`.

**Context-dump:** esf dump `r0/a1:  0x<..>  r1/a2:  0x<..>` … `xpsr: 0x<..>` (arch-flavored register names) · `Current thread: <addr> (<name>)` · `Fault during interrupt handling`.

**Terminators:** `Halting system` · reboot via `sys_reboot` → boot banner (reset marker).

**Edge behaviors:** fault decode lines arrive *before* the FATAL banner — the framer must retro-attach preceding fault lines to the record (lookback buffer); log-processed vs printk paths differ in prefix; shell prompt `uart:~$` mid-stream.

### A.7 FreeRTOS (`freertos` profile — heuristic + dialects)

Core FreeRTOS prints **nothing** on failure by default: `configASSERT`, `vApplicationStackOverflowHook`, and `vApplicationMallocFailedHook` are user/vendor-defined. The profile therefore ships a heuristic base plus **vendor dialect sub-profiles**, with ESP-IDF first (dominant in the wild):

**ESP-IDF dialect:** trigger `Guru Meditation Error: Core <n> panic'ed (<cause>)` (causes: `LoadProhibited`, `StoreProhibited`, `IllegalInstruction`, `IntegerDivideByZero`, `Interrupt wdt timeout on CPU<n>`, `Cache disabled but cached memory region accessed`, `Double exception`, `Unhandled debug exception`) · `***ERROR*** A stack overflow in task <t> has been detected.` · `assert failed: <func> <file>:<line> (<expr>)` · `abort() was called at PC 0x<addr> on core <n>` · `E (<ms>) <tag>:` / `W (<ms>) <tag>:` log lines · Brownout: `Brownout detector was triggered` · context: `Core  <n> register dump:` + `PC      : 0x<..>` rows + `Backtrace: 0x<..>:0x<..> 0x<..>:0x<..>` single-line backtrace · terminator: `Rebooting...` + `rst:0x<..> (<reason>),boot:0x<..>` reset-cause banner (excellent boot-loop signal).

**Generic heuristic tier:** `[Aa]ssert` + `file`/`line` co-occurrence · `[Ss]tack overflow` · `[Mm]alloc failed` · task-name-in-parens conventions — plus the cross-cutting A.10 classes. Expectation set in §5 stands: per-project declarative profiles are the norm here.

### A.8 ThreadX (`threadx` profile — heuristic + dialects)

Same situation as FreeRTOS: ThreadX itself is silent on failure (`_tx_` error codes are returned, not printed; `TX_STACK_ERROR` etc. reach the console only via user `tx_thread_stack_error_notify` handlers). Profile = A.10 generic classes + vendor dialects (STM32Cube/Azure RTOS sample harness prints, vendor HardFault handlers dumping Cortex-M `SCB->CFSR/HFSR/BFAR/MMFAR` values — those register-name tokens are the reliable trigger). Corpus-driven per-project profiles expected; the declarative tier (§5) is the deliverable that makes this cheap.

### A.9 LAVA / SQUAD seed sets

Port verbatim during Phase 2 (GPL-2.0+ → patterns with attribution, per §5): LAVA `LinuxKernelMessages` pexpect set (panic / oops / BUG / warning / trace — boot-scoped in LAVA, whole-stream in conminer) and SQUAD `linux_log_parser.py` full-log regex family (panic, oops, warning, kernel BUG, invalid opcode, unhandled fault, cut-here exception, trace). Consistency check in CI: conminer's `linux` profile must classify every corpus sample at least as severely as LAVA/SQUAD do (superset guarantee — we may find more, never less).

### A.10 Cross-cutting generic pattern classes

These are project-independent and form the `raw`-profile heuristics, the fallback for unknown firmware, and shared building blocks referenced by every profile above:

| Class | Shape | Used for |
|---|---|---|
| `SEVERITY_TOKEN` | `ERROR`/`WARN(ING)`/`FATAL`/`PANIC`/`BUG`/`FAIL(ED)`/`ASSERT` as word tokens, any case, incl. single-letter prefixes (`E:`, `E/`, `E (`) | severity classification everywhere |
| `ASSERT_LOC` | assert-keyword + `<path>:<line>` or `file ... line ...` co-occurrence | trigger class, all projects |
| `FILE_LINE` | `[\w/.-]+\.(c\|h\|rs\|cpp):\d+` | assert/oops locators |
| `HEX_ADDR` | `0x[0-9a-fA-F]{4,16}` | context-dump density detection |
| `REGISTER_LINE` | ≥2 `<reg-name><sep>0x<hex>` pairs per line, reg-name ∈ {`r\d+`, `x\d+`, `pc`, `lr`, `sp`, `elr`, `esr`, `far`, `RIP`, `RSP`, `RAX`…, `xpsr`, `cfsr`…} | CONTEXT-DUMP state, all projects |
| `BACKTRACE_HDR` | `(Call [Tt]race\|Backtrace\|Call stack\|Stack:)` | BACKTRACE state entry |
| `BACKTRACE_FRAME` | `[<hex>]`-style, `func+0xoff/0xsize`, or bare/paired hex frames | BACKTRACE continuation |
| `BANNER_VERSION` | product-name + dotted version + optional build date/hash | stage detection, reset markers |
| `RESET_MARKER` | earliest-stage banner recurrence, or explicit `[Rr]eset\|[Rr]ebooting\|Resetting CPU\|rst:` | record close + boot-loop counter |
| `WATCHDOG` | `watchdog\|wdt\|WDT` + `timeout\|reset\|bite\|bark` | involuntary-reset attribution (incl. Qualcomm-style bark/bite) |
| `COUNTDOWN_OVERWRITE` | CR/backspace-rewritten numeric field | linesplit handling (autoboot, spinners) |
| `KEY_VALUE_RUN` | ≥3 `\w+=[^ ]+` tokens per line | tokenizer rule (§6), env dumps |
| `DEAD_AIR` | configurable silence after an open crash record | timeout-close (UEFI/TF-A spin) |
| `GARBAGE_BURST` | sustained non-printable/invalid-UTF-8 ratio above threshold | baud-mismatch quarantine (§12.3) |

### A.11 Extended catalog — additional projects a lab console will see

Tier-2 profiles: all expressible in the A.0 tuple, delivered through the declarative tier (§5) as corpus arrives. Items marked **[vendor-corpus-required]** have per-SoC/per-generation formats — seed patterns below are indicative, and the profile ships only once real captures land in `corpus/`.

**A.11.1 Qualcomm platform stack [vendor-corpus-required]** — the highest-value gap for this lab.
- **PBL/XBL/SBL:** PBL numeric error codes on terminal failure; XBL/SBL banner + versioned image load lines; `Error code` + address dumps on auth/load failure; fallthrough to **EDL** (device drops to Sahara/9008 — console silence + host-side USB re-enumeration is the only marker, i.e. a DEAD_AIR + reset-marker case).
- **TZ/QSEE/QTEE:** secure-world fatal prints interleaved into the shared UART (overlay dialect, like OP-TEE).
- **remoteproc/SSR inside Linux dmesg (nested dialect):** `remoteproc remoteproc<n>: crash detected in <subsys>` · `qcom_q6v5 <dev>: fatal error received:` + firmware-supplied crash string · watchdog **bark**/**bite** vocabulary · `subsys-restart: Restarting <subsys>` (downstream kernels) · minidump/ramdump collection markers · rproc recovery lines. Record = SSR trigger through recovery/ramdump completion, attributed to the co-processor, not the kernel.
- **Co-processor firmware via drivers:** `ath10k/ath11k/ath12k ... firmware crashed!` + crash-dump register blocks · `brcmfmac: ... firmware halted` · PIL/load failures (`failed to load` + image name).

**A.11.2 Android over serial (`android` profile)**
- **Bootloaders:** LK/ABL panic + fastboot-mode entry banner · `ERROR: Could not do normal boot.` · AVB verification errors · `dm-verity device corrupted` (+ eio/restart policy lines).
- **Userspace:** tombstone header block `*** *** *** *** ***` · `Fatal signal <n> (SIG<name>), code <n>` · `F DEBUG   : backtrace:` + `#00 pc <addr>  <lib>` frames · `init: service '<name>' exited with status <n>` · init bootloop/`Fatal reboot` handling · logcat-over-console `F/` priority lines.

**A.11.3 Boot & secure firmware, extended**
- **coreboot:** stage banners `coreboot-<ver> ... bootblock starting` / `romstage` / `ramstage` (model stage-machine citizen) · `BUG:` / `ERROR:` prefixed lines · die/hang on fatal (DEAD_AIR close).
- **depthcharge / vboot:** `VB2:` prefixed lines · recovery-reason codes · verification-fail messages.
- **Barebox:** banner `barebox <ver>` · panic + register dump closely mirroring kernel arm format.
- **MCUboot:** `[INF]`/`[WRN]`/`[ERR]` prefixes · `Image in the primary slot is not valid!` · `Unable to find bootable image` · swap/upgrade state lines (stage precursor to Zephyr).
- **GRUB:** `error: <msg>` · `grub rescue>` prompt (trigger: bootability lost) · `Welcome to GRUB!` banner.
- **OpenSBI (RISC-V):** banner `OpenSBI v<ver>` + platform block · `sbi_trap_error` with `mcause`/`mtval`/`mepc` register dump · hart state lines.
- **Arm SCP-firmware:** `[FWK]` module prefixes · framework panic/error codes.
- **TF-M (Cortex-M sibling of TF-A):** `FATAL ERROR:` · SecureFault/security-violation reports + Cortex-M register dump · NS/S boundary fault attribution.
- **Hafnium:** VM/vCPU abort reports with ESR/FAR decode **[vendor-corpus-required]** in practice.

**A.11.4 RTOS, extended**
- **NuttX:** `up_assert: Assertion failed at file:<f> line <n>` · arch hardfault prints · register + stack dumps + task list dump on assert — full crash grammar, inherited by PX4/ArduPilot consoles.
- **RT-Thread:** `hard fault on thread: <name>` · `psr: 0x...` register block · thread list dump · assert `(<expr>) assertion failed at function:<fn>, line number:<n>`.
- **Mbed OS:** framed block `++ MbedOS Error Info ++` … `-- MbedOS Error Info --` · `Error Status: 0x<code>` · `Error Message:` · optional `++ MbedOS Fault Handler ++` register section — one of the most formal grammars in the catalog.
- **VxWorks:** task-level exception reports (`Exception ... task: <tid>`) · ED&R error records · shell banner.
- **ChibiOS:** system halt with panic-message pointer (terse — DEAD_AIR heavy) · debug-build assert file:line.
- **RIOT:** `*** RIOT kernel panic:` + panic string · `FAILED ASSERTION` + file:line · process dump.
- **MicroPython / CircuitPython:** Python `Traceback (most recent call last):` on the REPL console · `MemoryError:` · CircuitPython safe-mode banner + reason · auto-reload messages (record-boundary noise to suppress).

**A.11.5 Hypervisors & microkernels**
- **Xen:** `(XEN)` line prefix (overlay dialect over dom0 Linux) · `(XEN) Panic on CPU <n>:` · `----[ Xen-<ver> ... ]----` register/trace block · `Reboot in five seconds...` terminator · `Domain <n> crashed` guest notices.
- **Gunyah [vendor-corpus-required]:** sparse console by design; assert/fault prints vary by build — corpus from this lab's boards is the only honest seed. Fallback: A.10 classes + attribution when neither TZ nor kernel dialect matches.
- **seL4:** kernel fault prints (`vm fault on ...`, `cap fault in ...`) · rootserver/user fault dumps · debug-build aborts.
- **Zircon / Fuchsia:** `ZIRCON KERNEL PANIC` banner + crashlog · `{{{bt:<n>:<addr>}}}` / `{{{module:...}}}` / `{{{mmap:...}}}` symbolizer markup — machine-parseable by design; extract as structured fields, not just raw · panic-halt terminator.

**A.11.6 Linux userspace tier (extends `linux` profile, stage: userspace)**
- **systemd/init path:** `[FAILED] Failed to start <unit>` · `Dependency failed for` · emergency-mode banners · watchdog expiry lines.
- **initramfs/dracut:** `dracut-initqueue: ... timeout` · `Warning: /dev/... does not exist` · `Entering emergency mode` (pre-pivot stage marker).
- **glibc/allocator:** `*** stack smashing detected ***: terminated` · `double free or corruption` · `malloc(): invalid size` / `corrupted size vs. prev_size` · `Segmentation fault (core dumped)` shell echo.
- **Language runtimes (increasingly the crash that matters on embedded Linux):** Rust `thread '<name>' panicked at <file>:<line>:` + numbered backtrace frames · Go `panic: <msg>` + `goroutine <n> [running]:` stacks + runtime `fatal error:` · Python `Traceback (most recent call last):` … `<Error>: <msg>` (frames-then-trigger — inverted record shape, needs the Zephyr-style lookback) · C++ `terminate called after throwing an instance of '<type>'` + `what():` · Java/ART `FATAL EXCEPTION:` where applicable.
- **Container runtimes:** `OCI runtime create failed`, dockerd/containerd panics (Go grammar applies).

**A.11.7 Chrome EC (`cros-ec` profile)** — for any EC-carrying hardware on the bench: RO/RW image banner · `=== PROCESS EXCEPTION` + Cortex-M register dump · `Watchdog!` · `Reset cause:` line (first-class reset attribution) · console prompt `> ` interleave.

**Catalog governance:** A.11 entries graduate to §5 built-in status when their corpus + goldens land (per §12.2's contribution rule); until then they exist as declarative profiles in `profiles.d/`. The superset guarantee of A.9 extends across the whole appendix: any pattern a listed upstream tool detects, conminer must also detect.

**Framer contract derived from this appendix:** every profile is expressible as (banner set, severity map, trigger set, context-dump matcher, backtrace matcher, terminator set, reset markers) plugged into the A.0 state machine — which is precisely the declarative TOML schema of §5. If a future firmware can't be expressed in that tuple, the meta-grammar (not the firmware) gets amended, with a test.
