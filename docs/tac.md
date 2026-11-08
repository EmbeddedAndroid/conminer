# TAC ("Alpaca") board controllers

A Qualcomm TAC is one FT4232H whose four channels are split by its EEPROM:
channels A and B are the board's UART consoles, C and D are 8-bit GPIO ports
wired to the SoC's power, reset and strap lines. conminer drives the GPIO
channels natively (`conminer tac-power`), recognises a board by its USB
product string, and keeps the GPIO channels out of the console list: opening
one as a tty asserts DTR and RTS, which is a write to those lines.

## Built-in boards

| Product string | Controller | Vendor config | Modes |
|---|---|---|---|
| `RIDE MICRO 4.0` | `tac` | `TAC_FTDI_37.tcnf` | EDL, SAIL_EDL, UEFI, FASTBOOT |
| `ALPACA-LITE MTP DEBUG BOARD` | `tac-mtp` | `TAC_FTDI_13.tcnf` | EDL, UEFI, FASTBOOT, EUD, USB_OFF, USB_ON |

The two wire power differently, so each has its own sequence family:

- **RIDE**: one level, `pwr_off`, holds the board off; EDL straps
  `sw_dnld` and `ss_force_edl` across the power-up. `md_resout` is read back
  as a power sense.
- **MTP (Alpaca-lite)**: off is `battery`, `usb0` and `usb1` all cut; on
  restores them and presses `pkey` for 800 ms. EDL holds `pedl` through the
  power-up and a 1300 ms key press; UEFI holds `volup` and the key for 20 s;
  FASTBOOT holds `voldn` from before power-on until `fastboot_hold_ms` after
  the power-key release (see below). `TC_READY_N` enables the board-side outputs and is
  set inside each sequence, after the power-off levels, so the board never
  sees a stale level. There is no feedback line: `power-state` reports
  `unknown` with the commanded state beside it. EUD holds the `eud` line
  from before power-on and leaves it held, so the early boot loader enables
  the Embedded USB Debugger; `clear` and every other power-on release it.
  USB_OFF and USB_ON cut and restore only the two VBUS feeds (`usb0`,
  `usb1`) while `battery` keeps the board running, so its USB controller
  sees the host go away (Linux refuses to suspend with a host attached).
  They are line modes: `boot_mode` runs them without opening a boot epoch
  (and its dry run lists no `would_open_epochs`), and any power-on or boot
  mode restores the normal levels.
  `pshold` forces the SoC's PS_HOLD high (vendor initial value: released).
  Held, it hides every PS_HOLD drop from the PMIC, so the board can never
  reset or power off by itself; every sequence that switches power releases
  it, and nothing asserts it.

Sequences are transcribed from the vendor's `.tcnf` scripts, except the
MTP's EUD mode, which the vendor script does not have, and the MTP's
FASTBOOT hold. The transcriptions and their timings are in
`crates/conminer-core/src/tac.rs`.

## FASTBOOT on the MTP

The vendor's `bootToFastboot` releases volume down 1.8 s after the power-key
press, which suits a boot loader that reads the key that early. U-Boot reads
it only after its storage and USB scans, measured at about 5 to 6 s after the
power-key release, and never saw it. conminer keeps volume down held for
`fastboot_hold_ms` after the release instead (default 10000), then lets go
once, without the vendor's closing re-press:

    off; pkey 0; volup 0; voldn 1; pedl 0
    on;  pkey 1; delay 800; pkey 0
    delay fastboot_hold_ms; voldn 0

The hold is a controller setting, so a node can tune it without a rebuild:

    [[controllers]]
    name = "tac-mtp"
    boot_mode = "conminer tac-power mode {mode} --fastboot-hold-ms {fastboot_hold_ms} --device {device}"
    fastboot_hold_ms = 10000

`conminer tac-power mode FASTBOOT --fastboot-hold-ms N --dry-run --device
<console>` prints the exact pin sequence without driving anything. Holds over
50000 ms are refused: the whole sequence must fit in the hook timeout.

### Verifying the entry

A FASTBOOT entry is proven by the board's own fastboot device, not by the
hook's exit code. Name the file holding the serial the board's fastboot
gadget presents (U-Boot's `serial#`, or the stock boot loader's) on the
board's console:

    [devices."/dev/serial/by-id/usb-FTDI_ALPACA-LITE_MTP_DEBUG_BOARD_<serial>-if00-port0"]
    fastboot_serial_file = "/exports/<board>/fastboot-serial"

`boot_mode FASTBOOT` then waits up to `hooks.fastboot_settle_s` (default 20)
after the sequence for a USB device carrying the fastboot interface (class
0xff, subclass 0x42, protocol 0x03) with exactly that serial. The serial alone
is not enough, because the board's running OS can present it too.

- Found: the response carries `fastboot: {found: true, usb_id, port,
  waited_ms}`.
- Not found: the call fails with `FASTBOOT_NOT_ENTERED`. The board WAS
  actuated; `detail.boot_id` names the boot it started, and
  `detail.fastboot.seen` lists the fastboot devices that were on the bus
  (another board's, or this board's with a different serial).
- No file configured, or unreadable or empty: the mode runs and says
  `fastboot: {checked: false, why}`.

The file is read on every call, so updating it when the boot loader changes
needs no restart.

## Physical switches

On the MTP the Alpaca-lite only drives the board when the `ALPACA DISABLE`
switch (S5-6 on the label) is off. With it on, every action reports success
and nothing happens.

## Another board type

A board whose `.tcnf` names its product in `usb_descriptor` loads from
`/etc/conminer/tac.d` (or `CONMINER_TAC_DIR`) without a rebuild; pins named
`battery` and `pkey` select the MTP sequences, anything else the RIDE ones.
It also needs a controller profile whose glob matches the product string in
the by-id name.
