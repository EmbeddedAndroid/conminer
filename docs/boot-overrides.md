# Boot overrides: what a controller holds across boots

Some board controllers latch a boot-mode line. Select `BOOT_MD_EDL` on a Bantam
and it asserts `MD_EDL` and keeps it asserted: through resets, through power
cycles, until something releases it. That is deliberate. A flash needs the
board to come back into EDL after every reset for as long as the flash runs.

It is also a trap. A line left asserted sends the board into ROM EDL on every
boot, with a silent console, and nothing on the board says why. It looks like
dead firmware. conminer could set that line and clear it. It could not show it.

## Two facts, reported apart

| | What it is | Where it comes from |
| --- | --- | --- |
| `boot_overrides` | What the controller will make the board do on its **next** boot | Read back from the controller |
| `edl`, `capture_state: away_in_edl` | What the board is doing **now** | Observed on USB and the console |

Either can be true without the other. A line asserted a second ago has not put
the running board anywhere yet. A board can drop into EDL on its own with
nothing held. They are never merged into one indicator.

A read that fails is `unknown`. It is never shown as "nothing held": a
controller that did not answer has not said the next boot is a normal one.

## Seeing it

**Web UI.** Every controller panel has a *Held across boots* row: one chip per
held line (`MD_EDL held`), or `none held`, or `unknown`, with the age of the
controller reading. It refreshes with the normal status sweep. Looking at it
never changes a board: the sweep sends mcpd read-only questions and nothing else.

**MCP.**

    boot_overrides {"target": "2.4"}
    boot_overrides {"device": "AP", "max_age_s": 0}     # force a fresh read

returns the level of each line (`1`, `0`, or `null` for unknown), which are
`asserted`, the summary `state` (`latched`, `clear`, `unknown`, or `unsupported`
when the controller has no read hook), when the controller was read, and one
sentence on what that means for the next boot. No lease is needed.

`diagnose` carries the same object as `boot_overrides`, and when a line is held
its verdict says so and names the way out. `boot_mode` answers with a readback
taken after the change, and warns when more than one line is held at once:
selecting a mode asserts its own line and releases nothing, and an EDL line
decides the boot in ROM before UEFI or fastboot ever run.

A read holds a single-session controller for a few seconds, and every dashboard
on the fleet asks for it on a timer. So a reading may be served up to 20 s old
(`max_age_s` changes that), and every response says how old its reading is and
whether it was `controller_read` or `cached_controller_read`. Anything conminer
does to a line replaces the reading with its own readback at once. The only
thing the age can delay is news of a change made outside conminer.

## Booting normally

`power cycle` releases nothing, on purpose. To leave a latched mode:

**Web UI:** *Normal boot*, in the controller panel's Recover row.

**MCP,** holding the lease:

    normal_boot {"target": "2.4"}

It does three things under one actuation claim, so nothing else can touch the
board in between:

1. releases every override (`boot_mode clear`),
2. reads the controller back, fresh, and requires every line to read released,
3. power cycles, verified like any other power action.

If step 1 fails, or step 2 cannot show every line released (a line still held,
a line unreadable, the controller not answering), it stops with
`NORMAL_BOOT_ABORTED`. **Power is not cycled.** `detail.step` names the step,
`detail.boot_overrides` says what the controller holds now, and
`actuation_status` records the abort. `dry_run: true` shows the three commands
without running any of them.

`boot_mode clear` followed by `power cycle` still works as two calls. It is the
same release without the proof and without the single claim.

A controller that latches but has no read hook is refused outright: a normal
boot that cannot be verified is not the thing this tool promises. A controller
that sequences a mode itself and holds nothing (the Bughopper) needs no readback
and is cycled after the release.

## Flashing

Nothing here releases a line behind your back. `power on`, `off`, `cycle` and
`reset` never touch an override, and neither does any status read, so an EDL
held for a flash stays held until `boot_mode clear` or `normal_boot` is called.

## Teaching a controller to report

A controller profile gains one optional hook:

    [[controllers]]
    name = "bantam"
    boot_overrides = "bantam-power boot-overrides --port {controller}"

The command prints one line per override and must only ever query:

    MD_EDL=1
    SS_EDL=0
    UEFI=unknown

`0` is released, `1` is held, anything else is unknown. Lines that are not
`NAME=value` are ignored, so a status trailer is fine. `{controller}` and
`{device}` are substituted as for `power`. The shipped `bantam-power` reads the
same list of lines that its `mode clear` releases, so the two cannot disagree
about what "the overrides" are.
