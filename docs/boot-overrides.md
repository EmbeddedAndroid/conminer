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

`modes` answers the same question per boot mode, which is what a caller selects:

    "modes": {"BOOT_MD_EDL": {"line": "MD_EDL", "state": "held"},
              "BOOT_UEFI":   {"line": "UEFI",   "state": "released"}},
    "release": "per_mode"

`held` and `released` are each proven by a clean read of the mode's own line;
anything else is `unknown`. A mode that is missing holds nothing. After a failed
read `modes` is empty: nothing is known, so nothing is reported as released.

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

## Taking back one mode

`boot_mode clear` releases every line. That is the wrong tool for taking back
one mistaken selection on a board that is deliberately held in another mode,
EDL for a flash being the usual case.

**MCP,** holding the lease:

    boot_mode {"target": "2.4", "mode": "BOOT_UEFI", "release": true}

It releases the one line that mode holds, leaves the others as they are, and
answers with a fresh readback; a `warning` says so when the controller does not
read the mode back released. It is an actuation like any other `boot_mode`:
lease, one actuation at a time per board, `dry_run`. `release` with `clear` is
refused.

A controller whose profile has no `boot_mode_release` hook reports
`"release": "all_only"`. There the call is refused with `HOOK_NOT_CONFIGURED`.
It is never widened into a clear on its own.

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
held for a flash stays held until `boot_mode clear`, a `release` of that mode,
or `normal_boot` is called.

## Teaching a controller to report

A controller profile gains two optional hooks:

    [[controllers]]
    name = "bantam"
    boot_overrides    = "bantam-power boot-overrides --port {controller}"
    boot_mode_release = "bantam-power mode-release {mode} --port {controller}"

The read prints one line per override and must only ever query:

    MD_EDL=1
    SS_EDL=0
    UEFI=unknown
    mode BOOT_MD_EDL asserts MD_EDL
    mode BOOT_UEFI asserts UEFI

`0` is released, `1` is held, anything else is unknown. `{controller}` and
`{device}` are substituted as for `power`.

The `mode <MODE> asserts <LINE>` lines are optional and say which boot mode
holds which line. They are what `modes` is built from, and
they come from the hook because the hook is what turns a mode into a line when
it sets one: a second copy of that table in a config file is a copy that can
light the wrong button. Without them the lines are still reported and no mode
is shown as held. Anything else the command prints is ignored, so a status
trailer is fine.

`boot_mode_release` releases the one line `{mode}` holds and nothing else, and
must refuse a mode that holds none.

The shipped `bantam-power` sets, reads, releases one and releases all from a
single mode-to-line table, so they cannot disagree about what "the overrides"
are, and a test sets every shipped mode and reads it back to hold the table to
what the hardware-facing command actually does.
