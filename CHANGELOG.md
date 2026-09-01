# Changelog

All notable changes to conminer are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and CI fails a PR that
changes behaviour without adding an entry (§14.9).

## [Unreleased]

### Added

- **Evidence attachment and a merged timeline** (§18.1) — `attach_evidence` lets
  another tool (JTAG, a power sampler, a flash log, CI) hand conminer a fact with
  a timestamp, landing on whichever boot epoch covers it; `timeline` interleaves
  it with stage transitions and crashes in time order. A console is one evidence
  stream and silicon answers usually live in the join; payloads are stored
  verbatim because conminer does not understand a JTAG dump and must not pretend
  to.
- **Bisect** (§18.2) — `bisect_start` / `bisect_report` / `bisect_status` /
  `list_bisects`. conminer keeps the bookkeeping and can classify a candidate
  straight from an epoch via a predicate; the lab's own tooling still does the
  flashing. A range that is entirely skipped returns `inconclusive` rather than
  naming a build, and a non-monotonic result is surfaced as evidence the failure
  is intermittent instead of being smoothed over.
- **The silicon decoder ring** (§18.3) — `decode` resolves errno names, AArch64
  `ESR_ELx` classes and fault status, PSCI codes, GIC INTID/SPI arithmetic (the
  off-by-32 between a device tree and a register dump), and addresses against a
  new per-device `memory_map`. Ambiguous values return every reading with the
  assumption it rests on; nothing with no known meaning is guessed at.
- **Absence detection** (§18.4) — `learn_expectations` fits the skeleton of a
  normal boot from reference epochs and `missing_in_boot` reports what a suspect
  epoch did not print, which is the shape a bring-up failure usually takes and
  the one thing a novel-template list structurally cannot say. Reliability is
  carried rather than thresholded away, and a line that printed *late* is a
  separate finding from one that never printed.
- **The provenance guard** (§18.5) — `provenance` compares what the console said
  about itself against what was last claimed to be flashed, by hook or by
  `set_image`, so debugging a stale image is caught rather than discovered hours
  later. A mismatch is only claimable when both sides are known; otherwise the
  answer is `unknown`, never `match`.

- **Persistent triage** — `annotate_template` records a standing verdict
  (`benign` / `known_bad` / `investigating` / `interesting`) with a note and a
  ticket, and `list_verdicts` reads them back. `list_templates` hides benign
  rows by default and always reports `hidden_by_verdict`; `evaluate_policy`
  takes its allowlist from the store instead of demanding one on every call.
  Before this an agent's judgement lived only in its context window, so every
  session re-read and re-derived the same table of contents.
- **`template_values`** — the numbers inside a template's `<*>` slots as a
  series: min, max, mean, first/last, distinct values and samples. No-masking
  makes the wildcard the measurement; Drain computed those positions while
  clustering and discarded them. Values are re-derived from the stored bytes
  through the same profile and tokenizer, and an occurrence that cannot be
  aligned is counted as `unaligned` rather than guessed at.
- **Baselines** — `set_baseline` blesses an epoch, `list_baselines` lists them,
  and `list_templates(vs_baseline=…)` answers "what is new versus the last boot
  that *worked*", which session-scoped `new_only` cannot. `boot_report` gains a
  `vs_baseline` summary.
- **`diff_boots`** — two epochs compared including **stage timings**. A boot
  fingerprint is deliberately blind to duration, so "it still boots but handoff
  is 400 ms slower" was invisible to every other tool.
- **Durable watches** — `create_watch` / `poll_watch` / `list_watches` /
  `delete_watch`. `follow` is a long poll and loses a firing that happens while
  the agent is away; a watch is evaluated against the stored stream, so every
  firing survives with its own timestamp and offset. Polls are consuming, so
  nothing is delivered twice.
- **The human dashboard** (§17, `dashd`, port 8080) — live console list driven
  by server-sent events, per-console connection strings, and a browser terminal
  that watches and can transmit. One ser2net client per device is fanned out to
  every viewer.
- `ser2net.max_connections` (default 8) is emitted into the generated config.
  ser2net refuses a second client without it, which had quietly made the
  "several tools can share a console" premise false.
- `./cm image` builds the production runtime image. Only `./cm build` existed
  and it builds the *dev* image, which made it easy to smoke-test a stale
  `conminer:<version>`.

### Changed

- **`list_templates` and `list_boots` default to a compact projection.**
  Measured on a 109-template device the full template row was ~510 bytes
  against ~55 bytes for the average console line it stood for, so a 10x dedup
  win was handed straight back as per-row metadata — `tokens` alone was 18% of
  the response and is a second copy of `text`. The table of contents fell from
  55,584 to 19,723 bytes (2.8x); the boot list from 17,458 to 10,885 (1.6x).
  `view: "full"` reproduces the previous payload exactly, and every dropped
  field is still carried by `template_detail`.

### Fixed

- The provenance check compared the intended image against the epoch's *bound*
  image, which `set_image` had just written from the same claim. Every check
  passed by construction: the exact false all-clear the guard exists to prevent.
  It now compares against the console only.
- `diff_boots` panicked (`attempt to negate with overflow`) when a stage
  appeared in only one of the two epochs. Such stages now sort last, as a
  distinct finding rather than a zero-size one.
- Two browsers opening the same console simultaneously each opened their own
  ser2net connection: the check-then-dial in the dashboard's attach path was not
  serialised, so a console could quietly burn several of its eight client slots.

## [0.2.0]

First implementation of the v0.2 spec.

### Added

- **Core mining pipeline** — `bytes → lines → framer → Drain → SQLite`, shared
  byte-for-byte between live capture and post-hoc `ingest_file`.
- **Drain without masking** (§6) — fixed-depth prefix tree, similarity threshold,
  `<*>` overflow node. Wildcards appear only where real lines disagreed.
  Fragmentation is surfaced as merge suggestions, never auto-applied.
- **Framer profiles** (§5, Appendix A) — declarative TOML over the A.0
  meta-grammar; `linux`, `uboot`, `uefi`, `tfa`, `optee`, `zephyr`, `freertos`,
  `threadx`, `mcuboot`, `android`, `cros-ec`, `raw`. Overlay dialects, boot-stage
  tracking, retro-attachment, DEAD_AIR close.
- **Store** (§7) — SQLite WAL, one database per device plus a registry, embedded
  forward-only migrations, retention pruning, export/import archives.
- **Search** (§8.1) — FTS5 phrase and terms at line and record scope, regex with
  token-boundary-safe index pre-narrowing, windowed cross-boundary scan.
- **MCP server** (§8) — 46 tools over streamable HTTP and stdio, hard caps,
  cursors, structured errors, a freshness envelope on every read, and
  resource-updated notifications with storm coalescing.
- **Live services** (§3) — `discoveryd`, ser2net config generation and
  supervision, `minerd` capture with attested capture health, `mcpd`.
- **Boot epochs** (§8.4) — `mark`, detected resets, semantic fingerprints,
  `boot_report` classification, and the loop taxonomy.
- **Interactive runner** (§8.3) — per-character echo verification, prompt
  discipline, and the full hung-command recovery ladder.
- **Lab integration** (§15) — leases, exclusive binary claims, power and flash
  hooks, build identity and `diff_builds`, multi-console targets, LAVA and pstore
  ingestion, console file transfer, auto-baud selection, `evaluate_policy`,
  best-effort symbolization.
- **Test harness** (§12) — testkit with corpus, pty replay and fault injection;
  property tests for raw preservation, framer conservation, Drain determinism,
  rebuild idempotence and chunking independence; an e2e gate that asserts a
  device is visible and mined within 2 s of plug-in. `tools/list` is pinned to
  the exact 46-tool spec surface, so a registry that silently stopped short
  fails a test instead of leaving tools unreachable at runtime.
- `./cm image` builds the production runtime image. Previously only `./cm build`
  existed and it builds the *dev* image, which made it easy to smoke-test a
  stale `conminer:<version>`.

### Known deviations

- Ingest measures ~3.4 MB/s on the aarch64 development box rather than the
  100 MB/s the spec sets for its reference runner. The 800 MB benchmark stays
  `#[ignore]`d as the §12.5 nightly gate rather than being weakened to pass.
- Discovery polls `/dev/serial/by-id` rather than subscribing to udev netlink.
  The §11 fallback is promoted to the primary path on purpose: netlink
  availability inside a container varies by host distro, and a mechanism that
  works everywhere at 1 Hz is worth more than one that is silently dead on some
  hosts.
