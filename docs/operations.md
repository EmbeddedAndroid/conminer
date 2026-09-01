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

## Files a node owns

`cm sync` never overwrites these, and they must not be copied between nodes:

- `.env`: the node name, the address peers should dial, and bind addresses.
  Overwriting it renames this host in every peer's table, and device rows key
  on that name.
- `instance.json`: this node's identity.
- `/data`: the per-device stores.

## Health

    curl -s localhost:8080/healthz          # dashd
    curl -s localhost:8080/api/reports      # build fingerprint and the report queue
    docker ps --format '{{.Names}} {{.Status}}'

Every service except `peerd` carries a healthcheck. A node that answers
`/api/reports` with the expected `build` is serving the code you think it is.

## Storage

Stores grow with use. `stats` reports `db_bytes`, line and template counts, and
what the retention policy would collect. Pruning happens only when `prune` is
called; a configured policy alone does not reclaim anything.
