# Running a conminer node

Every node runs the same compose stack and owns its own consoles. Nothing is
installed on the host: the toolchain lives in `Dockerfile` and the compose
files, and `cm` is the only entry point you need.

## The services

| Service | Job |
| --- | --- |
| `discoveryd` | Watches `/dev/serial/by-id`, decides what is a console, generates the ser2net config |
| `ser2net` | Stock upstream ser2net, fans each tty out over TCP so several consumers can attach at once |
| `minerd` | Captures every console, frames and mines it, republishes the raw stream to the broker |
| `mcpd` | The agent surface (MCP), leases, actuation, queries |
| `dashd` | The human surface: device list, web console, reports |
| `peerd` | Announces this node to the fleet and answers peers |

## Deploying

    ./cm sync user@host          # push the source tree, excluding node-owned files
    ./cm up                      # build and start, on the node

`./cm build-id` prints the fingerprint of the source tree. It is baked into
every binary at image build time, so a node can always say which code it is
running. Two nodes reporting the same Cargo version can still be running
different builds; the fingerprint is the thing to compare.

Keep every node on the same build. A fleet where one node is behind produces
answers that disagree for reasons that have nothing to do with the boards.

`conminer.toml` is bind-mounted from the checkout, and the config rejects keys
it does not know. A newer file under an older image stops every service at its
next restart, so sync and `./cm up` in one pass, and do not leave a node with
the new file and the old image.

## Files a node owns

`cm sync` never overwrites these, and they must not be copied between nodes:

- `.env`: the node name, the address peers should dial, and bind addresses.
  Overwriting it renames this host in every peer's table, and device rows key
  on that name. Node policy that must not live in the shared `conminer.toml`
  goes here too: `CONMINER_RUNNER_ALLOW_RAW_SEND=true` enables the raw `send`
  passthrough on this node only.
- `instance.json`: this node's identity.
- `site.d/`: the node's own config (below).
- `/data`: the per-device stores.

## Node-local config

`conminer.toml` documents the compiled defaults and is the same on every node
(the `config` suite holds it to that). What belongs to one node goes in
`site.d/*.toml` beside it instead: a console's `[devices."<selector>"]`
section and hooks, a `[credentials] file`, anything that names this node's
hardware or paths. Every service loads `conminer.toml` and then each site file
in name order. Tables merge key by key, so a site file names only what it
changes; any other value, an array such as `[[controllers]]` included,
replaces the one before it. A typo in a site file stops the service like one
in `conminer.toml`, and the error names the file. `conminer check-config`
checks the layered result.

    # site.d/node.toml
    [credentials]
    file = "/var/lib/conminer/credentials.toml"

    [devices."/dev/serial/by-id/usb-FTDI_Example-if00-port0".hooks]
    flash = "/bin/sh /exports/example/flash.sh {image}"

Compose mounts the directory at `/etc/conminer/site.d` (`CONMINER_SITE_DIR`
names another one). Only its README is tracked, a deploy never overwrites it,
and `./cm build-id` does not hash it, so nodes with different site files still
report the same build for the same source. A change to a site file takes
effect when the services restart (`./cm up`).

## Health

    curl -s localhost:8080/healthz          # dashd
    curl -s localhost:8080/api/reports      # build fingerprint and the report queue
    docker ps --format '{{.Names}} {{.Status}}'

Every service except `peerd` carries a healthcheck. A node that answers
`/api/reports` with the expected `build` is serving the code you think it is.

## A board that keeps coming up in EDL

A powered board with a silent console that returns to EDL after every power
cycle is usually not broken firmware. Look for a lit boot-mode button and the
*Held across boots* row (in the controller panel, or under the open console),
or call `boot_overrides`: a latching controller keeps a boot-mode line asserted
until it is released, and `power cycle` releases nothing. *Normal boot* (or `normal_boot` over MCP) releases every line, proves
it by readback, and only then cycles. See [boot-overrides.md](boot-overrides.md).

## A console at a login prompt

`run_command` at a login prompt returns `LOGIN_REQUIRED` until a login for that
console is in the file named by `credentials.file`. The file must be mode 0600
and owned by the user conminer runs as; it is read at each gate, so adding an
entry needs no restart. Format, placement and what is refused:
[credentials.md](credentials.md).

## Storage

Stores grow with use. `stats` reports `db_bytes`, line and template counts, and
what the retention policy would collect. Pruning happens only when `prune` is
called; a configured policy alone does not reclaim anything.
