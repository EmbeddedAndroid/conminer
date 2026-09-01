# conminer — next session handoff

## The one blocker behind almost everything left

**ser2net 3.5.1 serves ONE client per port.** minerd holds it, so the dashboard,
`run_command`, `send` and `follow`'s prompt checks all get "Port already in use".

### The fix, with a working reference on this host

`~/hil` solves this in production: its runtime stage is **Debian trixie**, whose
`ser2net` package is **4.6.4-1**. Alpine stable has only 3.5.1, and both
`ser2net@edge` and `apk --repository .../edge/community` were tried here — apk
resolved 3.5.1 either way.

1. Switch the runtime stage to `debian:trixie-slim` + `apt-get install ser2net`
   (check the Rust target: a musl-static binary runs fine on Debian).
2. Then flip the config generator back to 4.x YAML — **but not the form it had**.
   Compare against `~/hil/web/ser2net.py:generate_yaml`, which is the correct
   4.x shape:
     - `accepter: telnet(rfc2217=false),tcp,<port>`   (conminer emitted bare `tcp,`)
     - `connector: serialdev,<path>,<line>,<flow>`     COMMA separated
     - conminer emitted SPACE separated v3 words (`115200 8DATABITS NONE ...`)
       inside 4.x YAML — a hybrid that is wrong on both versions.
   The v4 generator written earlier this session had this same defect, so do not
   simply restore it.
3. `max-connections: 8` then actually works and the dashboard can attach.

### Hardening to fold in from ~/hil/web/ser2net.py
- SIGHUP reload preserves sessions on unchanged accepters (already the intent
  in conminer's supervisor, but untested).
- Post-SIGHUP grace window so a watchdog does not kill a reloading ser2net
  (`_last_sighup_at`).
- Debounced regeneration + stable port assignment across restarts
  (`compute_port` hashes the port key rather than counting up).
- Crash-on-fd-churn: devices appearing/disappearing quickly. `_spawn` /
  `_drain_output` show the supervision shape.

## Fixed this session (deployed, verified on hardware)
- ser2net config format matched to the installed 3.x package (0 -> 4 listeners)
- discoveryd never wrote the config unless the device set CHANGED
- `-RTSCTS -XONXOFF LOCAL` muted the port on 3.x (0 -> 10k bytes)
- console endpoint address derived from ser2net's BIND, so every service
  resolved a sibling container to its own loopback. Was fixed three times in
  three copies; now ONE `Config::ser2net_host()` used by minerd, dashd, mcpd
  and the dashboard proxy.
- epoch attribution: `power` epochs were 0 bytes; minerd now adopts
  externally-opened epochs (verified: `boot 28 opened_by=power bytes=527`)

## Verified on hardware
- live capture 2054 lines / 68 templates
- power control by signal readback; MD_PS_HOLD 1->0->1
- dashboard controller panel + button path
- absence detection; search; decode; boot classification (`boot_looping`,
  `commandable: false` — correct prompt discipline)

## Still blocked by the BOARD, not software
`ERR: Platform power on failed / error: 13` on the main die. No AP console, so
no kernel versions, oops/crash detection, shell commands on Linux, or EDL
enumeration. EUD strap sets and reads back (EUD=1) but exposes no debug USB
device without the die running.

## Not started
- OpenOCD port PR for the EUD on-chip debugger
- secondary-die boot modes
- a framer profile for the SAIL firmware (stage detection reports `unknown`,
  correctly — no shipped profile matches it)
