# conminer

An agentic control plane for serial consoles and power control of embedded
devices.

An agent debugging an embedded target should never page a multi-megabyte log
through its context window. conminer gives it a **table of contents**
(deduplicated templates with counts and timelines) and lets it drill into
verbatim raw bytes only where it matters.

```
$ conminer ingest boot.log
session 1  device file:/logs/boot.log
  2.4 KB in 41 lines, 25 records, 23 templates (23 new)
  compression 1.8x  ·  codec plain
  stages: kernel → userspace
  crash records: 1
```

Apache 2.0 licensed. Everything runs in containers; no toolchain is installed
on the host.

Further reading: [docs/operations.md](docs/operations.md) to run a node,
[docs/capture.md](docs/capture.md) for the console capture path, and
[docs/spec.md](docs/spec.md) for the design.

---

## Quick start

```bash
./cm build          # build the dev image
./cm image          # build the production runtime image
./cm test           # run the whole suite, in a container
./cm up             # bring up the stack
./cm logs minerd
```

Then open **http://<lab-host>:8080** for the human dashboard, and point an agent
at **http://<lab-host>:8090/mcp** for the MCP surface.

Plug in a USB serial cable and it is queryable within about a second, with no
configuration:

```bash
./cm run devices
./cm run templates --order severity
```

### Attaching an agent

Streamable HTTP (remote agents, CI, Claude Code on a laptop):

```json
{
  "mcpServers": {
    "conminer": { "type": "http", "url": "http://127.0.0.1:8090/mcp" }
  }
}
```

stdio (local, single agent):

```json
{
  "mcpServers": {
    "conminer": {
      "command": "docker",
      "args": ["exec", "-i", "conminer-mcpd", "conminer", "mcpd", "--stdio"]
    }
  }
}
```

---

## Architecture

Five services under `docker-compose.yaml`, one shared volume:

| Service | Does |
|---|---|
| `discoveryd` | Watches `/dev/serial/by-id`, owns the device registry, generates `ser2net.yaml` |
| `ser2net` | Stock upstream ser2net; one TCP endpoint per device, multi-consumer |
| `minerd` | Live capture → framing → Drain → SQLite, plus file-ingest jobs |
| `mcpd` | MCP server: streamable HTTP and stdio, plus `/healthz` and `/metrics` |
| `dashd` | The human dashboard (§17): live console list, connection strings, watch and transmit |

Each of these runs in its own container, which matters in exactly one place: a
URL you hand to conminer is resolved from INSIDE that container, so `127.0.0.1`
means the service itself. A watch webhook pointing at a receiver on the lab host
needs the host's LAN address (or a compose service name), never loopback.

The tool surface covers the read side (`list_templates`, `template_detail`,
`get_records`, `get_context`, `search`, `boot_report`, `diff_sessions`,
`diff_builds`, `stats`), the live loop (`follow`, `mark`, `power`, `flash`,
`run_command`, `console_state`), device management (`acquire`/`release`,
`name_device`, `tag_device`, `set_line`, `classify_prompt`), multi-console
targets, file transfer, and `evaluate_policy` for CI gating. Persistent triage
(`annotate_template`), wildcard-slot measurement (`template_values`), baselines,
`diff_boots` and durable watches are covered below.

ser2net is the composition point: conminer never claims exclusive ownership of a
`/dev/tty*`, so minicom, uart-mcp, labgrid and conminer can all attach to the
same console at once.

### The pipeline

```
bytes → lines → FRAMER (records) → DRAIN (templates) → SQLite (WAL)
```

Live capture and post-hoc `ingest_file` feed the **identical** pipeline, so a
mined log file is queryable through exactly the same tools as a live console.

---

## What it guarantees

**Raw bytes are never rewritten.** No masking, no redaction, no normalization.
Templates, records, stages, search indexes and fingerprints are all derived
views that can be rebuilt from raw at any time — which is also how a similarity
threshold is retuned retroactively. The guarantee is an executable property test:
for any byte sequence `x`, `read(store(x)) == x`, including invalid UTF-8, NULs,
ANSI and torn lines.

**Framing is a partition, not a filter.** Every line belongs to exactly one
record, records are contiguous and ordered, and concatenating all record spans
reproduces the input exactly. A framer that silently dropped a crash record
would be the worst failure this system could have, so it is checked.

**An answer describes its own currency.** Every read response carries a
freshness envelope: `boot_id`, `last_rx_ts`, `idle_ms`, `bytes_this_boot`,
`capture_state`. An agent that just power-cycled a board and sees an unchanged
`boot_id` *knows* it is looking at stale data.

**"No output" and "I don't know" are different answers.** `no_output` is only
claimable when capture health was attested — port open, read loop alive. Without
that attestation the answer is `unknown_capture`, never silence.

**Errors are structured.** Every failure is `{code, message, hint, detail}`, so
an agent branches on `code` and acts on `detail` instead of parsing prose.

---

## Framer profiles

Profiles are declarative TOML in `profiles.d/`, bind-mounted so they are editable
without a rebuild. Each fills in the same meta-grammar:

```
[BANNER]* → normal traffic → TRIGGER → CONTEXT-DUMP* → BACKTRACE*
          → TERMINATOR? → (RESET-MARKER | silence)
```

Shipped: `linux`, `uboot`, `uefi`, `tfa`, `optee`, `zephyr`, `freertos`,
`threadx`, `mcuboot`, `android`, `cros-ec`, `raw`.

Develop one against your own logs without touching Rust:

```bash
./cm run profile test linux /logs/mine.log
```

A profile PR must include corpus samples and goldens for every edge case it
claims to handle.

---

## Non-goals

Documented from day one, so nobody builds on an assumption that will not hold:

- **Full-screen TUI interaction** over the command runner (`menuconfig`, `vi`).
  Line-oriented transactions only.
- **Masking or redaction of any kind.** This is the central constraint, not an
  omission.
- **Symbol-less binary reverse engineering.**
- **Being a flashing tool.** Power and flash are external-command hooks; the
  lab's tooling stays the lab's.
- **Replacing labgrid or LAVA scheduling.** conminer integrates and defers.
- **Authentication.** Explicitly out of scope for now; `mcpd` binds `127.0.0.1`
  and serial input is treated as untrusted regardless.

---

## The dashboard (§17)

`dashd` serves a page for the person at the bench, on port 8080:

- every console currently plugged in, updating by server-sent events as things
  are unplugged and replugged (an event fires only when the set really changes,
  so a steady lab is quiet and the change animation means something);
- each console's `telnet host port` / `nc host port` string, one click to copy;
- a live terminal you can **watch**, and switch to **send** when you need to
  type. Keystrokes go straight to ser2net, so they work even if the miner is
  wedged.

Two properties are worth knowing because they are choices, not accidents:

- **One ser2net client per console, however many people watch.** ser2net's
  budget is eight clients and minerd holds one permanently, so `dashd` owns a
  single connection per device and fans it out over WebSockets.
- **Nothing locks the line, and transmitted bytes are not mined.** Two browsers,
  or a browser and an agent, can interleave keystrokes into one UART; an agent
  reading the mined stream sees the echo with no record of who caused it. The UI
  says so rather than implying the console is yours alone. Set
  `dashboard.allow_tx = false` to make the whole surface read-only.

It binds the lab network with no authentication, which is why it is a separate
service on its own port: a host that should serve only agents does not run it.

## Reading less: the response budget

The table of contents only beats reading the log if a row is cheaper than the
lines it stands for. Two things make that true:

- `list_templates` and `list_boots` return a **compact** projection by default —
  what triage branches on, at roughly a third of the tokens. `view: "full"`
  restores every field, and `template_detail` carries them anyway.
- `annotate_template(..., verdict: "benign")` removes a row entirely, and the
  saving grows as you understand a device. Nothing is hidden silently: every
  response reports `hidden_by_verdict`.

## Silicon bring-up (§18)

Five things that matter when you are the one doing the bring-up:

- **`attach_evidence` / `timeline`** — conminer as the spine other tools attach
  to. Hand it a JTAG halt or a rail measurement with a timestamp and it lands on
  the right epoch, next to the console events it has to be read against.
- **`bisect_start` / `bisect_report`** — which of these forty builds introduced
  the hang. conminer keeps the bookkeeping and can classify a candidate straight
  from an epoch; your flash tooling still does the flashing.
- **`decode`** — errno names, AArch64 ESR classes and fault status, PSCI codes,
  the GIC INTID/SPI off-by-32, and addresses against this board's `memory_map`.
  Ambiguous values come back with every reading and its assumption.
- **`learn_expectations` / `missing_in_boot`** — what a boot *should* have
  printed and did not. The novel-template list cannot answer this, and it is the
  shape most bring-up failures take.
- **`provenance`** — is this the image you think it is? Catches the flash that
  did not take before you spend an afternoon debugging the previous build.

## Testing

```bash
./cm test              # the whole workspace
./cm suite framer-linux
./cm check             # fmt + clippy -D warnings
./cm gate              # check + test + e2e
```

Every feature and every enumerated edge case ships with tests. Suites live under
`crates/conminer/tests/<name>/` and are named for the §13 catalog: `linesplit`,
`drain`, `store`, `ingest`, `config`, `search`, `tools`, `notify`, `stagemachine`,
`discovery`, `line`, `follow`, `runner`, `state`, `epochs`, `integration`, `e2e`,
and one `framer-*` suite per profile.

The `e2e` suite runs the real binaries against a faked `/dev` with ptys standing
in for boards, and asserts the demo contract: **a device is visible and being
mined within 2 s of plug-in**.

---

## Backup and restore

The data volume is the whole state. A snapshot of it is a complete backup:

```bash
docker run --rm -v conminer-data:/data -v "$PWD":/out alpine \
  tar czf /out/conminer-backup.tgz -C /data .
```

Restore by extracting into a fresh volume. The round trip is covered in the
`e2e` suite.

---

## Performance gates

Nightly (§12.5), not per-PR:

```bash
./cm bench                          # report the numbers
CONMINER_BENCH_GATE=1 ./cm bench    # enforce the floors
```

Covers ingest throughput, the 10 KB fixed-cost case, per-line live-path latency,
`list_templates` on a large session, and template-count growth as the
fragmentation health metric.

**Measured deviation, stated plainly:** on the aarch64 development box used here
(a NAT-ed host on Docker overlayfs) ingest runs at roughly 3.4 MB/s, not the 100 MB/s floor
the spec sets for its reference runner. The structural work that mattered is
done — one transaction per read block rather than per record, indexed
open-epoch lookup instead of a table scan on every reset, and no template
rewrite when nothing changed, together worth about 40× — and the remainder is
SQLite row, index and FTS cost per line on this hardware. The 800 MB benchmark
therefore stays `#[ignore]`d as the nightly gate it is described as, rather than
being weakened to pass here.
