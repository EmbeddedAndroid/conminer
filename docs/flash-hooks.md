# Flash hooks

conminer does not flash boards itself. The `flash` tool runs the device's flash
hook, opens a provisioning epoch and binds the image to the boots that follow.
The hook is the only part that touches the board's storage.

`hooks/qdl-flash` is the flash hook for Qualcomm boards in EDL (Sahara and
Firehose through `qdl`). It is one script for every board: what differs between
boards lives in a board file that belongs to the node. The image ships the
script and its manifests at `/usr/local/libexec/conminer/hooks/`, so a deploy
updates every node identically and `./cm build-id` covers them.

## What lives where

| What | Where | Tracked |
|------|-------|---------|
| The hook, the manifests it verifies against | `hooks/` | yes |
| Board files (storage, layout, programmer, accepted references) | `site.d/flash/*.board` | no, node-local |
| Which console uses which board file | `site.d/*.toml`, `[devices."<console>".hooks] flash` | no, node-local |
| qdl runtime, firehose loaders, fastboot runtime | the board's stage, e.g. `exports/<board>/runtime` | no, binaries |
| Staged images and their `flash.log` | `exports/<board>/<image-ref>/` | no |
| The device's EDL serial | `exports/<board>/edl-serial` | no |

The runtime and the loaders are binaries, and some loaders are not
redistributable, so they stay out of git. What is tracked is their manifest:
the hook verifies every file a manifest names before qdl runs, so a stage
cannot drift from what the fleet agreed on. `hooks/manifests/qdl-<version>.sha256`
covers a qdl runtime (the binary, its loader and its libraries);
`firehose-<12 hex>.sha256` covers one loader set and is named by the first 12
hex digits of its own sha256, so a board file names its loaders by content.

## Wiring a board

    # site.d/fleet.toml
    [devices."/dev/serial/by-id/usb-FTDI_Example-if00-port0".hooks]
    flash = "/usr/local/libexec/conminer/hooks/qdl-flash --board /etc/conminer/site.d/flash/example.board {image}"

Pass `--device {device}` as well when the board file sets `console`. conminer
substitutes `{image}` and `{device}` as separate argv entries, never through a
shell. The hook must finish within `hooks.flash_timeout_s` (600 s); its qdl
timeout (`timeout_s`, default 540) stays below that, so qdl never outlives it.

`flash {dry_run: true}` shows the hook argv without running it. The hook's
own `--dry-run` runs qdl as `qdl --dry-run`, which validates the set and the
programmer without USB traffic. `--check` verifies the board file and the
stage's runtime and loaders against their manifests, without an image.

The caller owns the lease and the boot-mode transitions: put the board in EDL
(or FASTBOOT for `fastboot-boot`) before the flash, and boot it afterwards.
A board with `reset no` stays in EDL after the hook, so several flashes can
follow one EDL entry.

## The board file

One setting per line, `#` starts a comment. Relative paths are relative to
`stage`.

    stage /exports/example          # staged images, serial file, runtime dirs
    storage ufs                     # qdl --storage: ufs, emmc, spinor, nand, nvme
    runtime runtime qdl-v2.8-46-g657f12f
    firehose firehose firehose-0123456789ab
    programmer prog_firehose_ddr.elf
    reset no                        # no: qdl --skip-reset, the caller boots the board
    debug no                        # yes: qdl --debug
    timeout_s 540
    edl_serial_file edl-serial

    ref kind FP-[0-9]{6}-[0-9]{6}
    ref xml STOCK-[A-Za-z0-9._-]+ own-programmer
    ref read READ-[A-Za-z0-9._-]+

    kind fip fip.elf 4/uefi_a 5/uefi_b
    kind tz tz.mbn 4/tz_a 5/tz_b

| Setting | Meaning |
|---------|---------|
| `stage <dir>` | Required, absolute. Image directories, the serial file and the runtime directories live here. |
| `storage <type>` | Required. qdl's storage type. |
| `runtime <dir> <manifest>` | Required. The qdl runtime and the manifest that covers it. |
| `firehose <dir> <manifest>` | The loader set used when an image directory carries no programmer. |
| `fastboot_runtime <dir> <manifest>` | A fastboot runtime, for `fastboot-boot`. |
| `programmer <name or glob>...` | How to find the programmer in a directory, first match wins. A Sahara XML is a programmer too; qdl runs from its directory because it names its loaders relative to it. |
| `reset yes\|no` | Whether qdl resets the board when its session ends. |
| `edl_serial_file`, `fastboot_serial_file` | The device serial qdl or fastboot selects. No serial, no flash. |
| `sector_bytes <n>` | Sector size for windows and digest readbacks (default 4096). |
| `console <path>` | When set, the hook refuses unless `--device` names this console. |
| `ref <mode> <regex> [policy]` | An accepted image reference and its mode. The first matching line wins; anything else is refused. `own-programmer` requires the directory's own programmer; `firehose-programmer` ignores one. |
| `kind <name> <file> <target>...` | For `kind`: writes `<file>` to each `<lun>/<label>`. Several lines with one name make a multi-payload kind, in line order. The first kind is the default. |
| `max_bytes <file> <n>` | A payload larger than this, or empty, is refused. |
| `window <name> <lun>/<start>+<count> r\|rw` | For `window-*`: a fixed sector range. |
| `nor_payload <file>` | For `window-write`: the payload file name. |
| `digest_dir <dir>` | For `digest`: where images are staged. |
| `digest <target> <write> <whole\|lun/start> <exact\|max> <bytes>` | For `digest`: the write address, the readback window (the whole write address, or a start sector whose length follows the image) and the size rule. |

## Modes

Every image directory carries `SHA256SUMS` over what it writes, and nothing
runs unless it verifies. A directory's own programmer must be listed in it.

- **kind**: `flash-kind.txt` names the kind (else the first). `SHA256SUMS.flash`
  lists exactly that kind's payloads, in order. All writes go in one qdl
  session.
- **xml**: `rawprogram*.xml` then `patch*.xml`, each in version order, with the
  files they name found through `--include`.
- **parts**: `partitions.txt` holds one `<lun>/<label> <file>` write per line,
  every file listed in `SHA256SUMS`; one session.
- **xml-or-parts**: xml when the directory has rawprogram XML, else parts.
- **read**: `partitions.txt` holds `<lun>/<label> <file>` or
  `<lun>/<start>+<count> <file>`; each is read into the directory, never over an
  existing file, and `SHA256SUMS.read` is written over the results.
- **window-write**: `part.txt` names one `rw` window; `SHA256SUMS` holds exactly
  the payload's line. The payload is padded to whole sectors, written, read
  back and compared in one session; success prints `READBACK MATCH`.
- **window-read**: `part.txt` names one window; it is read into `<name>.bin`
  and its length checked.
- **fastboot-boot**: `boot.img`, listed in `SHA256SUMS`, is booted once with
  `fastboot boot`. Nothing is written.
- **digest**: the reference is `v1:<target>:<sha256>:<file>`. The file in
  `digest_dir` must match the digest and the target's size rule. It is copied
  with the programmer into `runs/run-*` under the stage, written from there, and
  verified by an on-device `sha256` of the readback window; success prints
  `VERIFIED_SHA256=`.

Each run appends to `flash.log` in the image directory (`runs/run-*/flash.log`
for digest). Before qdl starts, the hook stops any qdl still running from the
same runtime: one orphaned by a hook that conminer timed out would otherwise
take the next EDL entry mid-Sahara. One flash runs per stage at a time.

## Adding a board

1. Stage the qdl runtime and the board's loaders under its stage directory.
   If the loader set is new, write its manifest (`sha256sum` over the files,
   sorted by name), name it `firehose-<first 12 hex of its own sha256>.sha256`
   and add it to `hooks/manifests/`; that is a source change and ships with the
   next deploy.
2. Write the board file in `site.d/flash/`. Take the LUN and label of every
   target from the board's own GPT (a `read` of `<lun>/0+6` per LUN) or its
   stock rawprogram XML, never from another board of the same SoC: layouts
   differ between boards.
3. Wire the console's `flash` hook in a site file and restart (`./cm up`).
4. Run `qdl-flash --board <file> --check` inside mcpd, then a `--dry-run`
   flash of a staged set, before the first real flash.
